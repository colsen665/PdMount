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
            .map(|(data, from)| (data.to_vec(), from)),
        Some((b"hello world".to_vec(), ReadFrom::Ram))
    );
    assert_eq!(
        hot(|| engine.read_local(handle, 11, 5))
            .expect("eof")
            .map(|(data, _)| data)
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
async fn a_remote_file_too_large_to_stage_is_efbig() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval) = open(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let file = found(hot(|| engine.lookup(ROOT_INO, b"top.bin")).expect("lookup"));
    // A size only the uploader's claim backs (S-5).
    engine.tree.write().get_mut(file.ino).expect("node").size = 1 << 62;

    let (handle, _) = engine.open_file(file.ino, WRITE).expect("open");
    let error = engine
        .write_local(handle, 1 << 61, b"x")
        .expect_err("staged");
    engine.release_file(handle);
    assert!(matches!(error, CoreError::FileTooLarge), "{error:?}");
    assert_eq!(error.errno(), nix::libc::EFBIG);
    assert!(!engine.staging.data_path(file.ino).exists());
    assert!(!engine.staged.lock().contains_key(&file.ino));
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
async fn the_kernel_keeps_directory_pages_only_while_the_entries_are_unchanged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval) = open(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let opendir = || {
        let (handle, keep) = engine.opendir(ROOT_INO).expect("opendir");
        engine.releasedir(handle);
        keep
    };
    assert!(!opendir(), "nothing served yet");
    assert!(opendir(), "the same entries");

    // A local change, and a remote one applied without the kernel.
    write_file(&engine, ROOT_INO, "local.txt", b"l");
    assert!(!opendir(), "a local change");
    assert!(opendir());
    let root = backend.root().await.expect("root").link_id;
    backend
        .upload_new(
            &root,
            "remote.txt",
            UploadSource::Bytes(Bytes::from_static(b"r")),
            meta(),
        )
        .await
        .expect("upload");
    engine.revalidate().await.expect("revalidate");
    assert!(!opendir(), "a remote change");

    // Another open handle could fill the cache from its own snapshot.
    let (held, _) = engine.opendir(ROOT_INO).expect("opendir");
    assert!(!opendir(), "another handle is open");
    engine.releasedir(held);
    assert!(!opendir(), "the pages may come from either snapshot");
    assert!(opendir());
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

/// Backend calls per method since the last reset (the latency counts).
fn backend_calls(engine: &Engine) -> Vec<(&'static str, u64)> {
    engine
        .metrics()
        .snapshot_latency()
        .into_iter()
        .filter(|(name, _)| name.starts_with("backend_"))
        .map(|(name, summary)| (name, summary.count))
        .collect()
}

/// The §5.7 rows that are network counts, not times: the first listing
/// costs one listing call, once; a prefetched small file's first byte no
/// call; a small file that was not prefetched one block map and one block.
#[tokio::test]
async fn first_bytes_and_listings_cost_the_slo_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = FakeBackend::new();
    let root = backend.root().await.expect("root").link_id;
    for name in ["small-a", "small-b"] {
        backend
            .upload_new(
                &root,
                name,
                UploadSource::Bytes(Bytes::from_static(b"small file")),
                meta(),
            )
            .await
            .expect("upload");
    }
    let (engine, _inval) = open(&backend, dir.path()).await;
    engine.metrics().reset_latency();

    engine.ensure_listed(ROOT_INO).await.expect("list");
    engine.ensure_listed(ROOT_INO).await.expect("listed");
    assert_eq!(backend_calls(&engine), [("backend_list_children", 1)]);

    // Not prefetched: no opendir, so no prefetch.
    engine.metrics().reset_latency();
    let a = found(hot(|| engine.lookup(ROOT_INO, b"small-a")).expect("lookup"));
    assert_eq!(read_file(&engine, a.ino).await, b"small file");
    let mut calls = backend_calls(&engine);
    calls.retain(|(name, _)| name.starts_with("backend_fetch_block"));
    assert_eq!(calls.iter().map(|(_, n)| n).sum::<u64>(), 1, "{calls:?}");
    assert_eq!(
        backend_calls(&engine)
            .into_iter()
            .filter(|(name, _)| !name.starts_with("backend_fetch_block"))
            .collect::<Vec<_>>(),
        [("backend_blocks", 1)]
    );

    // Prefetched by opendir (`small-a` is cached already): the read makes
    // no call.
    let b = found(hot(|| engine.lookup(ROOT_INO, b"small-b")).expect("lookup"));
    let (handle, _) = engine.open_file(b.ino, READ).expect("open");
    engine.opendir(ROOT_INO).expect("opendir");
    eventually("prefetch", || {
        matches!(engine.read_local(handle, 0, 100), Ok(Some(_)))
    })
    .await;
    engine.metrics().reset_latency();
    let (data, _) = hot(|| engine.read_local(handle, 0, 100))
        .expect("read")
        .expect("local");
    engine.release_file(handle);
    assert_eq!(&data[..], b"small file");
    assert_eq!(backend_calls(&engine), []);
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

/// A new file has no staging sidecar (D067): startup finds its staging by
/// the data file, not as missing content to recreate empty.
#[tokio::test]
async fn a_pending_new_file_without_a_sidecar_survives_a_crash() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let ino = {
        // No executor: the file is only local when the "daemon" dies.
        let (engine, _inval) = open(&backend, dir.path()).await;
        engine.ensure_listed(ROOT_INO).await.expect("list");
        let ino = write_file(&engine, ROOT_INO, "new.txt", b"no sidecar");
        engine.fsync(ino).await.expect("fsync");
        engine.store.crash();
        ino
    };
    let staging = dir.path().join("staging");
    assert!(staging.join(ino.to_string()).exists());
    assert!(!staging.join(format!("{ino}.state")).exists());
    let engine = running(&backend, dir.path()).await;
    assert_eq!(read_file(&engine, ino).await, b"no sidecar");
    settle(&engine).await;
    assert_eq!(fake_tree(&backend).await["/new.txt"].content, b"no sidecar");
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

/// `status` counts an entry's age from when it was queued, across a
/// restart (D105).
#[tokio::test]
async fn the_oldest_age_survives_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let ino = {
        // No executor: the entry stays queued.
        let (engine, _inval) = open(&backend, dir.path()).await;
        engine.ensure_listed(ROOT_INO).await.expect("list");
        let ino = write_file(&engine, ROOT_INO, "queued.txt", b"left behind");
        assert_eq!(engine.queue_counts().pending, 1);
        assert!(engine.queue_oldest().expect("oldest") < Duration::from_secs(5));
        engine.store.flush().await.expect("flush");
        // The entry was queued five minutes before the session ended.
        let before = unix_ms_now() - 300_000;
        engine.store.write(move |conn| {
            conn.execute("UPDATE queued SET queued_at_ms = ?1", [before])
                .map(drop)
        });
        engine.shutdown().await.expect("shutdown");
        ino
    };
    let (engine, _inval) = open(&backend, dir.path()).await;
    assert_eq!(engine.queue_counts().pending, 1);
    let oldest = engine.queue_oldest().expect("oldest");
    assert!(oldest >= Duration::from_secs(300), "{oldest:?}");
    assert!(oldest < Duration::from_secs(330), "{oldest:?}");
    // The entry uploads as usual and its row goes.
    engine.start_background();
    settle(&engine).await;
    assert!(engine.queue_oldest().is_none());
    assert_eq!(read_file(&engine, ino).await, b"left behind");
    engine.store.flush().await.expect("flush");
    let rows: i64 = engine
        .store
        .read(|conn| conn.query_row("SELECT COUNT(*) FROM queued", [], |row| row.get(0)))
        .expect("rows");
    assert_eq!(rows, 0, "no row for an entry that left");
}

fn unix_ms_now() -> i64 {
    crate::journal::unix_ms(std::time::SystemTime::now())
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
    assert_eq!(local.map(|(data, _)| data).as_deref(), Some(&content[..]));
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

/// D080, D110: a closed write gate refuses every change as `mount -o ro`
/// does; it opens and closes at runtime, and the kernel's cached modes are
/// invalidated each time.
#[tokio::test]
async fn the_write_gate_closes_and_opens_at_runtime() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let mut options =
        EngineOptions::in_dir(dir.path(), config(), test_device("dev-a"), fixed_clock());
    options.write_gate = false;
    let (engine, mut invalidations) = Engine::open(Arc::new(backend.clone()), options)
        .await
        .expect("engine");
    engine.start_background();
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let docs = found(engine.lookup(ROOT_INO, b"docs").expect("lookup")).ino;
    engine.ensure_listed(docs).await.expect("list docs");
    let file = found(engine.lookup(docs, b"a.txt").expect("lookup")).ino;

    assert!(!engine.write_gate_open());
    assert!(!engine.is_read_only(), "not a read-only mount");
    assert!(is_read_only(
        engine.create(docs, b"new", 0o644, true, WRITE)
    ));
    assert!(is_read_only(engine.mkdir(ROOT_INO, b"d", 0o755)));
    assert!(is_read_only(engine.open_file(file, WRITE)));
    assert!(is_read_only(engine.unlink(docs, b"a.txt")));
    assert!(is_read_only(engine.rename(
        docs,
        b"a.txt",
        ROOT_INO,
        b"out",
        RenameMode::Replace
    )));
    let chmod = SetAttr {
        mode: Some(0o777),
        ..SetAttr::default()
    };
    assert!(is_read_only(engine.setattr(file, &chmod)));
    assert!(is_read_only(engine.access(file, MAY_WRITE)));
    assert_eq!(engine.getattr(file).expect("attr").perm, 0o400);
    assert_eq!(
        engine
            .xattr(file, "user.pdmount.access")
            .expect("xattr")
            .as_deref(),
        Some("ro")
    );
    assert!(engine.open_file(file, READ).is_ok(), "reads still work");

    // Open: the full write path, and the modes the kernel cached go.
    while invalidations.try_recv().is_ok() {}
    engine.set_write_gate(true);
    let mut invalidated = Vec::new();
    while let Ok(message) = invalidations.try_recv() {
        invalidated.push(message);
    }
    assert!(
        invalidated.contains(&Invalidation::Inode { ino: file }),
        "{invalidated:?}"
    );
    assert_eq!(engine.getattr(file).expect("attr").perm, 0o600);
    write_file(&engine, docs, "ok.txt", b"allowed");
    let (handle, _) = engine.open_file(file, WRITE).expect("open for writing");
    settle(&engine).await;
    assert!(fake_tree(&backend).await.contains_key("/docs/ok.txt"));

    // Closed again: a handle opened before can no longer write (§11.4).
    engine.set_write_gate(false);
    assert!(is_read_only(engine.write_local(handle, 0, b"x")));
    engine.release_file(handle);
    // Setting the same state again changes nothing.
    while invalidations.try_recv().is_ok() {}
    engine.set_write_gate(false);
    assert!(invalidations.try_recv().is_err());
}

/// D114: a share's tree that follows the server: a refused change closes
/// the gate and holds the queue; the files stay readable, a local change
/// does not lift the hold, and access coming back uploads them.
#[tokio::test]
async fn a_refused_change_closes_the_gate_and_holds_the_queue() {
    use pdmount_backend::fake::FakeRole;
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeBackend::new();
    let added = fake
        .add_root("Team", Some("owner@example.test"), Some(FakeRole::Editor))
        .expect("share");
    let share = added.share.clone().expect("share id");
    let tree = fake.tree(&added.volume, added.root.clone());
    let mut options =
        EngineOptions::in_dir(dir.path(), config(), test_device("dev-a"), fixed_clock());
    options.cache.disk_bytes = 1 << 20;
    let (engine, _inval) = Engine::open(Arc::new(tree), options).await.expect("engine");
    let told = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hook = {
        let told = Arc::clone(&told);
        Arc::new(move |held: usize| {
            told.fetch_add(held.max(1), Ordering::SeqCst);
        })
    };
    engine.follow_write_refusals(hook);
    engine.start_background();
    engine.ensure_listed(ROOT_INO).await.expect("list");
    engine.reserve_inos(16).await.expect("inos");
    write_file(&engine, ROOT_INO, "a.txt", b"first");
    settle(&engine).await;

    // The owner makes the user a viewer; the next upload is refused.
    fake.set_own_role(&share, Some(FakeRole::Viewer))
        .expect("viewer");
    let b = write_file(&engine, ROOT_INO, "b.txt", b"second");
    let start = Instant::now();
    while engine.write_gate_open() && start.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!engine.write_gate_open(), "the refusal closes the gate");
    assert!(engine.write_was_refused());
    assert!(told.load(Ordering::SeqCst) >= 1, "the hook is told");
    let counts = engine.queue_counts();
    assert_eq!((counts.pending, counts.parked), (0, 1), "{counts:?}");
    assert_eq!(
        engine.parked(),
        [("/b.txt".to_owned(), rights::NO_WRITE_ACCESS.to_owned())]
    );
    assert!(is_read_only(
        engine.create(ROOT_INO, b"c.txt", 0o644, true, WRITE)
    ));
    assert_eq!(read_file(&engine, b).await, b"second", "still readable");
    // Reading and closing again does not lift the hold.
    let (handle, _) = engine.open_file(b, READ).expect("open");
    engine.release_file(handle);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(engine.queue_counts().parked, 1);

    // Editor again: the gate opens and the held change uploads.
    fake.set_own_role(&share, Some(FakeRole::Editor))
        .expect("editor");
    assert_eq!(engine.restore_write_access(), 1);
    assert!(engine.write_gate_open() && !engine.write_was_refused());
    settle(&engine).await;
    let names: Vec<String> = fake
        .tree(&added.volume, added.root.clone())
        .list_children(&added.root)
        .await
        .expect("list")
        .into_iter()
        .map(|node| node.name)
        .collect();
    assert!(names.contains(&"b.txt".to_owned()), "{names:?}");
}

/// D114: an engine that does not follow the server (My Files) parks only
/// the refused entry and keeps its gate open.
#[tokio::test]
async fn my_files_keeps_its_gate_when_a_change_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = running(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    backend.inject_failure(
        pdmount_backend::fake::FakeOp::UploadNew,
        BackendError::PermissionDenied,
    );
    write_file(&engine, ROOT_INO, "x.txt", b"x");
    let start = Instant::now();
    while engine.queue_counts().parked == 0 && start.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(engine.queue_counts().parked, 1);
    assert!(engine.write_gate_open());
    assert!(!engine.write_was_refused());
}

/// D114: the queued changes of a tree that goes away are written out with
/// their content, and a file whose base cannot be read is listed with the
/// reason.
#[tokio::test]
async fn queued_changes_are_exported_with_their_content() {
    use crate::engine::orphan::{FILES_DIR, OrphanManifest};
    let dir = tempfile::tempdir().expect("tempdir");
    let out = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let mut config = config();
    // Nothing uploads: no executor runs.
    config.upload.debounce = HumanDuration(Duration::from_secs(3600));
    let (engine, _inval) = open_with(&backend, dir.path(), config).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let docs = found(engine.lookup(ROOT_INO, b"docs").expect("lookup")).ino;
    engine.ensure_listed(docs).await.expect("list docs");
    let a = found(engine.lookup(docs, b"a.txt").expect("lookup")).ino;
    let top = found(engine.lookup(ROOT_INO, b"top.bin").expect("lookup")).ino;
    // An edit of a remote file whose base is read (cached) ...
    assert_eq!(read_file(&engine, a).await, b"hello world");
    let (handle, _) = engine.open_file(a, WRITE).expect("open");
    engine
        .write(handle, 11, b"!!".to_vec())
        .await
        .expect("append");
    engine.release_file(handle);
    // ... a new file in a new folder, and a delete.
    let sub = engine.mkdir(ROOT_INO, b"sub", 0o755).expect("mkdir").ino;
    write_file(&engine, sub, "new.txt", b"brand new");
    engine.unlink(docs, b"a.txt").expect("unlink");
    // The other remote file is only read.
    assert_eq!(read_file(&engine, top).await, b"0123456789");
    assert_eq!(engine.withdraw_write_access(), engine.journal.inos().len());

    let entries = engine.export_queued(out.path()).await.expect("export");
    let manifest = OrphanManifest::new("Team", "the share was revoked", entries);
    manifest.write(out.path()).expect("manifest");
    let back = OrphanManifest::read(out.path()).expect("read back");
    assert_eq!(back, manifest);
    let files = out.path().join(FILES_DIR);
    assert_eq!(
        std::fs::read(files.join("sub/new.txt")).expect("new.txt"),
        b"brand new"
    );
    assert!(files.join("sub").is_dir());
    let by_path = |path: &str| {
        manifest
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap_or_else(|| panic!("{path} in {:?}", manifest.entries))
            .clone()
    };
    assert_eq!(by_path("/sub").change, "create-folder");
    let new = by_path("/sub/new.txt");
    assert_eq!(new.change, "upload");
    assert_eq!(new.file.as_deref(), Some("files/sub/new.txt"));
    assert_eq!(manifest.incomplete(), 0, "{:?}", manifest.entries);
    // The deleted file is listed as a trash, without content.
    let trashed = by_path("/docs/a.txt");
    assert_eq!(trashed.change, "trash");
    assert!(trashed.file.is_none());
}

/// D114: an edit whose base blocks cannot be read any more is listed with
/// the reason instead of its content.
#[tokio::test]
async fn an_edit_whose_base_cannot_be_read_is_listed_with_the_reason() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = tempfile::tempdir().expect("tempdir");
    let backend = FakeBackend::new();
    let root = backend.root().await.expect("root").link_id;
    let size = crate::staging::GRID as usize + 10;
    backend
        .upload_new(
            &root,
            "big.bin",
            UploadSource::Bytes(Bytes::from(vec![7u8; size])),
            meta(),
        )
        .await
        .expect("upload");
    let mut config = config();
    config.upload.debounce = HumanDuration(Duration::from_secs(3600));
    let (engine, _inval) = open_with(&backend, dir.path(), config).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let big = found(engine.lookup(ROOT_INO, b"big.bin").expect("lookup")).ino;
    // An edit of the tail: the first staging block stays on the server.
    let (handle, _) = engine.open_file(big, WRITE).expect("open");
    engine
        .write(handle, crate::staging::GRID, b"tail".to_vec())
        .await
        .expect("write");
    engine.release_file(handle);
    engine.withdraw_write_access();
    for _ in 0..8 {
        backend.inject_failure(
            pdmount_backend::fake::FakeOp::FetchBlock,
            BackendError::NotFound,
        );
    }
    let entries = engine.export_queued(out.path()).await.expect("export");
    let entry = entries
        .iter()
        .find(|entry| entry.path == "/big.bin")
        .expect("big.bin");
    assert!(entry.file.is_none(), "{entry:?}");
    assert!(entry.error.is_some(), "{entry:?}");
    assert!(!out.path().join("files/big.bin").exists());
}

/// An engine of a tree on another user's volume (D124): the profile's
/// rules in `local`, the root's `/.pdignore` not taken.
async fn share_engine(
    backend: &FakeBackend,
    dir: &std::path::Path,
    config: Config,
    local: &[u8],
) -> Arc<Engine> {
    let rules = dir.join("profile.pdignore");
    std::fs::write(&rules, local).expect("profile rules");
    let mut options = EngineOptions::in_dir(dir, config, test_device("dev-a"), fixed_clock());
    options.cache.disk_bytes = 1 << 20;
    options.local_ignore = Some(rules);
    options.remote_ignore = false;
    let (engine, _inval) = Engine::open(Arc::new(backend.clone()), options)
        .await
        .expect("engine");
    engine
}

/// D124: in a tree on another user's volume, the root's `/.pdignore` is an
/// ordinary file. Its `*` keeps nothing of the member's local-only, its
/// `!*` undoes none of the member's own rules; the file itself syncs.
#[tokio::test]
async fn a_share_tree_takes_no_rules_from_its_root_pdignore() {
    let backend = seeded().await;
    let root = backend.root().await.expect("root").link_id;
    remote_file(&backend, &root, ".pdignore", b"*\n").await;
    // The same rules do apply in a tree of the user's own volume.
    let own_dir = tempfile::tempdir().expect("tempdir");
    let own = running(&backend, own_dir.path()).await;
    own.ensure_listed(ROOT_INO).await.expect("list");
    eventually("the root's rules in an own tree", || {
        own.is_ignored(ROOT_INO, "a.txt", false)
    })
    .await;
    own.shutdown().await.expect("shutdown");

    let dir = tempfile::tempdir().expect("tempdir");
    let engine = share_engine(&backend, dir.path(), config(), b"*.env\n").await;
    engine.start_background();
    engine.ensure_listed(ROOT_INO).await.expect("list");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !engine.is_ignored(ROOT_INO, "a.txt", false),
        "the share's `*`"
    );
    assert!(engine.is_ignored(ROOT_INO, "x.env", false), "the own rule");
    write_file(&engine, ROOT_INO, "a.txt", b"shared");
    let env = write_file(&engine, ROOT_INO, "x.env", b"secret");
    assert!(engine.tree.read().get(env).expect("x.env").local_only);
    // Another member turns the share's rules into `!*`.
    engine.unlink(ROOT_INO, b".pdignore").expect("unlink");
    write_file(&engine, ROOT_INO, ".pdignore", b"!*\n");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        engine.is_ignored(ROOT_INO, "y.env", false),
        "`!*` undoes nothing"
    );
    write_file(&engine, ROOT_INO, "y.env", b"secret too");
    settle(&engine).await;
    let tree = fake_tree(&backend).await;
    assert_eq!(tree["/a.txt"].content, b"shared");
    assert_eq!(tree["/.pdignore"].content, b"!*\n", "the rules file syncs");
    assert!(!tree.contains_key("/x.env"));
    assert!(!tree.contains_key("/y.env"));
    engine.shutdown().await.expect("shutdown");
}

/// V-1, D124: a gone tree's local-only files are exported like queued ones
/// (their content is only in the staging), listed as `local-only`, though
/// the queue is empty: the journal let them go.
#[tokio::test]
async fn local_only_files_are_exported_with_the_queued_changes() {
    use crate::engine::orphan::{FILES_DIR, LOCAL_ONLY, OrphanManifest};
    let dir = tempfile::tempdir().expect("tempdir");
    let out = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = share_engine(&backend, dir.path(), config(), b"*.env\nbuild/\n").await;
    engine.start_background();
    engine.ensure_listed(ROOT_INO).await.expect("list");
    write_file(&engine, ROOT_INO, "secret.env", b"local secret");
    let build = engine.mkdir(ROOT_INO, b"build", 0o755).expect("mkdir").ino;
    write_file(&engine, build, "out.o", b"object");
    settle(&engine).await;
    let counts = engine.queue_counts();
    assert_eq!(counts.pending + counts.parked, 0, "nothing queued");
    assert_eq!(engine.local_only_staged().len(), 2);

    engine.withdraw_write_access();
    let entries = engine.export_queued(out.path()).await.expect("export");
    let manifest = OrphanManifest::new("Team", "the share was revoked", entries);
    assert_eq!(manifest.incomplete(), 0, "{:?}", manifest.entries);
    let files = out.path().join(FILES_DIR);
    for (path, content) in [
        ("secret.env", &b"local secret"[..]),
        ("build/out.o", b"object"),
    ] {
        assert_eq!(std::fs::read(files.join(path)).expect(path), content);
    }
    let change = |path: &str| {
        manifest
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap_or_else(|| panic!("{path} in {:?}", manifest.entries))
            .change
            .clone()
    };
    assert_eq!(change("/secret.env"), LOCAL_ONLY);
    assert_eq!(change("/build/out.o"), LOCAL_ONLY);
    assert_eq!(manifest.entries.len(), 2, "{:?}", manifest.entries);
    assert!(!fake_tree(&backend).await.contains_key("/secret.env"));
    engine.shutdown().await.expect("shutdown");
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
        ..FakeClient::default()
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

/// Levels of the deep tree below `docs` (pjdfstest's PATH_MAX trees have
/// about 30).
const DEEP: usize = 10;

/// `docs/d0/d1/.../d9`, a file `f` in each folder. Returns the links, top
/// first.
async fn deep_remote_tree(backend: &FakeBackend, docs_link: &LinkId) -> Vec<LinkId> {
    let mut links: Vec<LinkId> = Vec::new();
    for level in 0..DEEP {
        let parent = links.last().unwrap_or(docs_link).clone();
        let link = backend
            .create_folder(&parent, &format!("d{level}"), UNIX_EPOCH)
            .await
            .expect("mkdir");
        backend
            .upload_new(
                &link,
                "f",
                UploadSource::Bytes(Bytes::from_static(b"f")),
                meta(),
            )
            .await
            .expect("f");
        links.push(link);
    }
    links
}

/// Lists the deep tree; returns its folders' inodes, `docs` first.
async fn list_deep(engine: &Arc<Engine>, docs: Ino) -> Vec<Ino> {
    let mut inos = vec![docs];
    for level in 0..DEEP {
        let parent = *inos.last().expect("docs");
        let name = format!("d{level}");
        let ino = found(hot(|| engine.lookup(parent, name.as_bytes())).expect("lookup")).ino;
        engine.ensure_listed(ino).await.expect("list");
        inos.push(ino);
    }
    inos
}

/// `rm -rf` of the levels `levels` (deepest first): the file, then the
/// folder.
fn remove_levels(engine: &Engine, inos: &[Ino], levels: impl Iterator<Item = usize>) {
    for level in levels {
        engine.unlink(inos[level + 1], b"f").expect("unlink");
        engine
            .rmdir(inos[level], format!("d{level}").as_bytes())
            .expect("rmdir");
    }
}

/// [`settle`] that shows what is left.
async fn settle_deep(engine: &Engine) {
    let counts = engine.drain(Duration::from_secs(10)).await;
    assert_eq!(
        (counts.pending, counts.parked),
        (0, 0),
        "{:#?}",
        engine.describe_pending()
    );
}

/// Each folder's trash waits for the folders deleted inside it, so a deep
/// tree goes to the trash one level at a time. A level must start as soon
/// as the one inside it is trashed: as a failing step with its back-off,
/// level n waited about 2^n seconds (minutes from the tenth level on).
#[tokio::test]
async fn a_deep_tree_removed_locally_is_trashed_level_after_level() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let root = backend.root().await.expect("root").link_id;
    let docs_link = remote_child(&backend, &root, "docs").await.link_id;
    let links = deep_remote_tree(&backend, &docs_link).await;
    let (engine, docs, _) = listed_lazy(&backend, dir.path()).await;
    let inos = list_deep(&engine, docs).await;
    remove_levels(&engine, &inos, (0..DEEP).rev());

    start_journal(&engine);
    settle_deep(&engine).await;
    for link in &links {
        assert!(is_trashed(&backend, link).await, "{link:?}");
    }
    assert!(engine.conflicts().is_empty(), "{:?}", engine.conflicts());
}

/// The report: an `rm -rf` cut off by a stop, finished after the next
/// start. The folders deleted before the stop wait for each other like
/// those deleted after it.
#[tokio::test]
async fn a_deep_rm_rf_cut_off_by_a_stop_is_trashed_after_the_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let root = backend.root().await.expect("root").link_id;
    let docs_link = remote_child(&backend, &root, "docs").await.link_id;
    let links = deep_remote_tree(&backend, &docs_link).await;
    let inos = {
        let (engine, docs, _) = listed_lazy(&backend, dir.path()).await;
        let inos = list_deep(&engine, docs).await;
        // Cut off halfway: the deeper half is gone locally, none trashed.
        remove_levels(&engine, &inos, (DEEP / 2..DEEP).rev());
        engine.shutdown().await.expect("shutdown");
        inos
    };
    let (engine, _inval) = open_with(&backend, dir.path(), lazy_config()).await;
    remove_levels(&engine, &inos, (0..DEEP / 2).rev());

    start_journal(&engine);
    settle_deep(&engine).await;
    for link in &links {
        assert!(is_trashed(&backend, link).await, "{link:?}");
    }
    assert!(engine.conflicts().is_empty(), "{:?}", engine.conflicts());
}

/// A remote file moved into a `.pdignore`d folder stays in its old folder
/// remotely: no node moves into a local-only folder (§7.7). When that old
/// folder is deleted, its trash cannot wait for the file to move out, nor
/// take it along: the folder is kept like one with a child the local delete
/// does not account for, and the server is not asked again. Its trash used
/// to wait on the file's entry, which left at once and woke it: two calls
/// about once a second, forever (D121).
#[tokio::test]
async fn a_folder_whose_child_went_into_a_local_only_folder_is_kept() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let root = backend.root().await.expect("root").link_id;
    let docs_link = remote_child(&backend, &root, "docs").await.link_id;
    let engine = running(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    write_file(&engine, ROOT_INO, ".pdignore", b"ignored/\n");
    eventually("the ignore rules", || {
        engine.is_ignored(ROOT_INO, "ignored", true)
    })
    .await;
    let ignored = engine
        .mkdir(ROOT_INO, b"ignored", 0o755)
        .expect("mkdir")
        .ino;
    let docs = found(engine.lookup(ROOT_INO, b"docs").expect("lookup")).ino;
    engine.ensure_listed(docs).await.expect("list docs");
    engine
        .rename(docs, b"a.txt", ignored, b"a.txt", RenameMode::NoReplace)
        .expect("into the local-only folder");
    engine.rmdir(ROOT_INO, b"docs").expect("rmdir");

    eventually("the folder is kept", || {
        kinds(&engine).contains(&ConflictKind::LocalDeleteRemoteChildren)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    engine.metrics().reset_latency();
    tokio::time::sleep(Duration::from_secs(3)).await;
    // A trash check is a `get_nodes` and a `list_children` of the folder;
    // applying an event may fetch one node.
    let calls = backend_calls(&engine);
    let count = |op: &str| {
        calls
            .iter()
            .find(|(name, _)| *name == op)
            .map_or(0, |(_, count)| *count)
    };
    assert_eq!(
        count("backend_list_children"),
        0,
        "the folder is not checked again: {calls:?}"
    );
    assert!(count("backend_get_nodes") <= 1, "{calls:?}");
    assert!(!is_trashed(&backend, &docs_link).await);
    let tree = fake_tree(&backend).await;
    assert!(tree.contains_key("/docs/a.txt"), "{:?}", tree.keys());
    assert!(!tree.contains_key("/ignored"));
    let a = found(hot(|| engine.lookup(ignored, b"a.txt")).expect("lookup")).ino;
    assert_eq!(read_file(&engine, a).await, b"hello world");
    found(hot(|| engine.lookup(ROOT_INO, b"docs")).expect("the folder is back"));
}

/// A deleted folder whose name a node took is trashed under a temporary
/// name (D117); until then its entry is shown, and found, at the folder's
/// own path, also after a restart (D121).
#[tokio::test]
async fn a_folder_moved_aside_keeps_its_path_in_the_queue() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let docs_path = || "/docs".to_owned();
    {
        let (engine, docs, _) = listed_lazy(&backend, dir.path()).await;
        let a = found(engine.lookup(docs, b"a.txt").expect("lookup")).ino;
        let mv = |from: Ino, name: &[u8], to: Ino, new: &[u8]| {
            engine
                .rename(from, name, to, new, RenameMode::NoReplace)
                .expect("rename")
        };
        mv(docs, b"a.txt", ROOT_INO, b"b");
        engine.rmdir(ROOT_INO, b"docs").expect("rmdir");
        mv(ROOT_INO, b"b", ROOT_INO, b"docs");
        // The file waits; the folder's trash goes on as far as it can.
        engine
            .queue_park(&queue::QueueTarget::Id(a))
            .await
            .expect("park");
        start_journal(&engine);
        eventually("the folder moved aside", || {
            remote_of(&engine, docs)
                .is_some_and(|remote| remote.name.starts_with(crate::journal::SWAP_PREFIX))
        })
        .await;
        let path_of = |entries: &[queue::QueueEntry]| {
            entries
                .iter()
                .find(|entry| entry.id == docs)
                .map(|entry| entry.path.clone())
        };
        assert_eq!(path_of(&queue_all(&engine).await), Some(docs_path()));
        // Once the file leaves the name, the path finds the folder's entry.
        mv(ROOT_INO, b"docs", ROOT_INO, b"x");
        let by_path = engine
            .queue_entries(&queue_path("/docs"))
            .await
            .expect("found by its own path");
        assert_eq!(path_of(&by_path), Some(docs_path()));
        engine.shutdown().await.expect("shutdown");
    }
    let (engine, _inval) = open_with(&backend, dir.path(), lazy_config()).await;
    let entries = queue_all(&engine).await;
    assert!(
        entries.iter().any(|entry| entry.path == docs_path()),
        "{:?}",
        entries.iter().map(|e| &e.path).collect::<Vec<_>>()
    );
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

// ---- pins (§5.3, D060) ----

/// An engine with only the pin task running (no event polling), unpaced.
async fn pinning(backend: &FakeBackend, dir: &std::path::Path) -> Arc<Engine> {
    let (engine, _inval) = open(backend, dir).await;
    engine.gate.set_interval(Duration::ZERO);
    engine.runtime().spawn(Arc::clone(&engine).run_pins());
    engine
}

async fn pin_complete(engine: &Arc<Engine>, path: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let pins = engine.pin_list().await.expect("list");
        let pin = pins
            .iter()
            .find(|pin| pin.path.as_deref() == Some(path))
            .unwrap_or_else(|| panic!("no pin on {path}: {pins:?}"));
        if pin.state == pins::PinState::Complete && pin.fetched_bytes == pin.total_bytes {
            return;
        }
        assert!(Instant::now() < deadline, "not complete: {pin:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn pinned_bytes(engine: &Engine) -> u64 {
    engine.cache_stats().pinned_bytes
}

#[tokio::test]
async fn a_pinned_folder_is_listed_fetched_and_survives_gc_and_wipe() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let root = backend.root().await.expect("root").link_id;
    let docs = backend
        .list_children(&root)
        .await
        .expect("list")
        .into_iter()
        .find(|node| node.name == "docs")
        .expect("docs")
        .link_id;
    let sub = backend
        .create_folder(&docs, "sub", UNIX_EPOCH)
        .await
        .expect("mkdir");
    backend
        .upload_new(
            &sub,
            "deep.txt",
            UploadSource::Bytes(Bytes::from_static(b"deep content")),
            meta(),
        )
        .await
        .expect("upload");
    let engine = pinning(&backend, dir.path()).await;
    engine.pin_set("/docs").await.expect("pin");
    pin_complete(&engine, "/docs").await;
    let pins = engine.pin_list().await.expect("list");
    assert_eq!(pins[0].files, 2);
    assert_eq!(pins[0].total_bytes, 11 + 12);
    let summary = engine.pin_summary().await.expect("summary").expect("pins");
    assert_eq!((summary.paths, summary.fetched_bytes), (1, 23));
    let pinned = pinned_bytes(&engine);
    assert!(pinned > 23);
    assert_eq!(engine.cache_stats().bytes, 0, "not counted against the cap");
    engine.cache_gc(Some(0)).await.expect("gc");
    engine.cache_wipe(None).await.expect("wipe");
    engine.cache_wipe(Some("/docs")).await.expect("wipe");
    assert_eq!(pinned_bytes(&engine), pinned);
    // A file outside the pin is not fetched.
    assert_eq!(engine.cache_stats().blocks, 3 + 3);
}

#[tokio::test]
async fn unpin_makes_blocks_evictable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = pinning(&backend, dir.path()).await;
    engine.pin_set("/top.bin").await.expect("pin");
    pin_complete(&engine, "/top.bin").await;
    assert!(pinned_bytes(&engine) > 0);
    engine.cache_gc(Some(0)).await.expect("gc");
    assert!(pinned_bytes(&engine) > 0);
    engine.pin_unset("/top.bin").await.expect("unpin");
    eventually("the unpin", || pinned_bytes(&engine) == 0).await;
    assert!(engine.pin_list().await.expect("list").is_empty());
    assert!(engine.pin_summary().await.expect("summary").is_none());
    assert!(engine.cache_gc(Some(0)).await.expect("gc").blocks > 0);
    let not_pinned = engine.pin_unset("/top.bin").await.expect_err("not pinned");
    assert!(
        not_pinned.to_string().contains("not pinned"),
        "{not_pinned}"
    );
}

#[tokio::test]
async fn unpin_inside_a_pinned_folder_names_the_pin() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = pinning(&backend, dir.path()).await;
    engine.pin_set("/docs").await.expect("pin");
    let error = engine.pin_unset("/docs/a.txt").await.expect_err("inside");
    assert!(
        error.to_string().contains("pinned through /docs"),
        "{error}"
    );
}

#[tokio::test]
async fn a_new_remote_revision_of_a_pinned_file_is_fetched_and_the_old_one_let_go() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = pinning(&backend, dir.path()).await;
    engine.sync_events().await.expect("cursor");
    engine.pin_set("/top.bin").await.expect("pin");
    pin_complete(&engine, "/top.bin").await;
    let file = found(engine.lookup(ROOT_INO, b"top.bin").expect("lookup"));
    let (link, old) = {
        let tree = engine.tree.read();
        let remote = tree
            .get(file.ino)
            .and_then(|n| n.remote.clone())
            .expect("remote");
        (remote.link, remote.rev.expect("rev"))
    };
    let new = backend
        .upload_revision(
            &link,
            &old,
            UploadSource::Bytes(Bytes::from_static(b"a newer revision")),
            meta(),
        )
        .await
        .expect("revision");
    engine.sync_events().await.expect("events");
    pin_complete(&engine, "/top.bin").await;
    assert!(engine.cache.on_disk(&BlockKey::new(&link, &new, 1)));
    // Only the old revision's blocks can go.
    let wiped = engine.cache_gc(Some(0)).await.expect("gc");
    assert_eq!(wiped.blocks, 3);
    assert!(!engine.cache.on_disk(&BlockKey::new(&link, &old, 1)));
    assert!(engine.cache.on_disk(&BlockKey::new(&link, &new, 1)));
}

#[tokio::test]
async fn our_own_upload_stays_pinned_without_a_download() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = {
        let engine = running(&backend, dir.path()).await;
        engine.gate.set_interval(Duration::ZERO);
        engine
    };
    engine.pin_set("/docs").await.expect("pin");
    pin_complete(&engine, "/docs").await;
    let docs = found(engine.lookup(ROOT_INO, b"docs").expect("lookup"));
    write_file(&engine, docs.ino, "new.txt", b"local content");
    settle(&engine).await;
    let before_upload = pinned_bytes(&engine);
    eventually("the upload's blocks pinned", || {
        pinned_bytes(&engine) > before_upload
    })
    .await;
    let calls = |engine: &Engine| {
        backend_calls(engine)
            .into_iter()
            .find(|(name, _)| *name == "fetch_block")
            .map_or(0, |(_, count)| count)
    };
    let before = calls(&engine);
    engine.cache_gc(Some(0)).await.expect("gc");
    let pins = engine.pin_list().await.expect("list");
    assert_eq!(pins[0].files, 2);
    assert_eq!(pins[0].fetched_bytes, 11 + 13);
    assert_eq!(calls(&engine), before);
}

#[tokio::test]
async fn a_pin_survives_a_restart_and_resumes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let pinned;
    {
        let (engine, _inval) = open(&backend, dir.path()).await;
        // No task yet: nothing is fetched before the restart.
        engine.pin_set("/top.bin").await.expect("pin");
        assert_eq!(pinned_bytes(&engine), 0);
        engine.shutdown().await.expect("shutdown");
    }
    {
        let engine = pinning(&backend, dir.path()).await;
        pin_complete(&engine, "/top.bin").await;
        pinned = pinned_bytes(&engine);
        assert!(pinned > 0);
        engine.store.flush().await.expect("flush");
        engine.shutdown().await.expect("shutdown");
    }
    let (engine, _inval) = open(&backend, dir.path()).await;
    assert_eq!(pinned_bytes(&engine), pinned, "pin = 1 read back");
}

#[tokio::test]
async fn a_rename_out_of_a_pinned_folder_unpins_and_one_in_pins() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = pinning(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let docs = found(engine.lookup(ROOT_INO, b"docs").expect("lookup"));
    engine.ensure_listed(docs.ino).await.expect("list");
    engine.pin_set("/docs").await.expect("pin");
    pin_complete(&engine, "/docs").await;
    assert!(pinned_bytes(&engine) > 0);
    engine
        .rename(
            docs.ino,
            b"a.txt",
            ROOT_INO,
            b"out.txt",
            RenameMode::Replace,
        )
        .expect("out");
    eventually("the move out", || pinned_bytes(&engine) == 0).await;
    engine
        .rename(
            ROOT_INO,
            b"top.bin",
            docs.ino,
            b"in.bin",
            RenameMode::Replace,
        )
        .expect("in");
    pin_complete(&engine, "/docs").await;
    eventually("the move in", || pinned_bytes(&engine) > 0).await;
}

#[tokio::test]
async fn deleting_a_pinned_node_deletes_its_pin() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = pinning(&backend, dir.path()).await;
    engine.pin_set("/top.bin").await.expect("pin");
    pin_complete(&engine, "/top.bin").await;
    engine.unlink(ROOT_INO, b"top.bin").expect("unlink");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !engine.pin_list().await.expect("list").is_empty() {
        assert!(Instant::now() < deadline, "the pin stayed");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(pinned_bytes(&engine), 0);
    engine.store.flush().await.expect("flush");
    let rows: i64 = engine
        .store
        .read(|conn| conn.query_row("SELECT count(*) FROM pins", [], |row| row.get(0)))
        .expect("read");
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn an_atomic_save_keeps_the_pin() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = pinning(&backend, dir.path()).await;
    engine.pin_set("/top.bin").await.expect("pin");
    pin_complete(&engine, "/top.bin").await;
    write_file(&engine, ROOT_INO, "top.bin.tmp", b"saved");
    engine
        .rename(
            ROOT_INO,
            b"top.bin.tmp",
            ROOT_INO,
            b"top.bin",
            RenameMode::Replace,
        )
        .expect("save");
    let pins = engine.pin_list().await.expect("list");
    assert_eq!(pins.len(), 1);
    assert_eq!(pins[0].path.as_deref(), Some("/top.bin"));
}

#[tokio::test]
async fn pin_is_refused_when_the_disk_cannot_hold_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = pinning(&backend, dir.path()).await;
    *engine.pins.test_free_bytes.lock() = Some((1 << 30) + 5);
    let error = engine.pin_set("/top.bin").await.expect_err("too large");
    assert!(error.to_string().contains("no space"), "{error}");
    assert!(engine.pin_list().await.expect("list").is_empty());
    // It fits: pinned, and the fetcher waits below the reserve.
    *engine.pins.test_free_bytes.lock() = Some((1 << 30) + 100);
    engine.pin_set("/top.bin").await.expect("fits");
    pin_complete(&engine, "/top.bin").await;
}

#[tokio::test]
async fn the_fetcher_stops_when_free_space_runs_out() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = pinning(&backend, dir.path()).await;
    // Folder contents are not known yet, so the pin is accepted on a full
    // disk; the fetcher finds out.
    *engine.pins.test_free_bytes.lock() = Some(1 << 20);
    engine.pin_set("/docs").await.expect("pin");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let pins = engine.pin_list().await.expect("list");
        if pins[0].state == pins::PinState::WaitingSpace {
            break;
        }
        assert!(Instant::now() < deadline, "{pins:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(engine.cache_stats().blocks, 0);
    *engine.pins.test_free_bytes.lock() = Some(1 << 40);
    engine.pins.nudge_for_tests();
    pin_complete(&engine, "/docs").await;
}

#[tokio::test]
async fn pin_fetches_are_paced() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = FakeBackend::new();
    let root = backend.root().await.expect("root").link_id;
    let folder = backend
        .create_folder(&root, "many", UNIX_EPOCH)
        .await
        .expect("mkdir");
    for i in 0..20 {
        backend
            .upload_new(
                &folder,
                &format!("f{i}"),
                UploadSource::Bytes(Bytes::from_static(b"x")),
                meta(),
            )
            .await
            .expect("upload");
    }
    let (engine, _inval) = open(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let many = found(engine.lookup(ROOT_INO, b"many").expect("lookup"));
    engine.ensure_listed(many.ino).await.expect("list");
    engine.pin_set("/many").await.expect("pin");
    let before = engine.metrics().backend_calls.load(Ordering::Relaxed);
    tokio::time::pause();
    engine.runtime().spawn(Arc::clone(&engine).run_pins());
    tokio::time::sleep(Duration::from_millis(1200)).await;
    // Turns at most every 0.5 s, each one file with two calls (block list,
    // block). Unpaced, all 20 files would be done.
    let calls = engine.metrics().backend_calls.load(Ordering::Relaxed) - before;
    assert!(calls == 6, "backend calls in 1.2 s: {calls}");
}

#[tokio::test]
async fn offline_a_pinned_file_reads_without_the_backend_and_fetching_waits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (backend, config) = faulty(dir.path()).await;
    let (engine, _inval) = open_with(&backend, dir.path(), config).await;
    engine.gate.set_interval(Duration::ZERO);
    engine.runtime().spawn(Arc::clone(&engine).run_pins());
    engine.pin_set("/top.bin").await.expect("pin");
    pin_complete(&engine, "/top.bin").await;
    pdmount_backend::fake::add_fault(&dir.path().join("fake"), offline_rule()).expect("fault");
    engine.health().set(Connectivity::Offline);
    let before = engine.metrics().backend_calls.load(Ordering::Relaxed);
    let file = found(engine.lookup(ROOT_INO, b"top.bin").expect("lookup"));
    assert_eq!(read_file(&engine, file.ino).await, b"0123456789");
    assert_eq!(
        engine.metrics().backend_calls.load(Ordering::Relaxed),
        before,
        "a pinned read needs no backend"
    );
    // A pin set offline, on a known path, waits for the connection.
    engine.pin_set("/docs").await.expect("pin");
    let pins = engine.pin_list().await.expect("list");
    let waiting = pins
        .iter()
        .find(|pin| pin.path_at_set == "/docs")
        .expect("pin");
    assert_eq!(waiting.state, pins::PinState::WaitingOffline);
    pdmount_backend::fake::clear_faults(&dir.path().join("fake"), None).expect("clear");
    engine.health().set(Connectivity::Online);
    pin_complete(&engine, "/docs").await;
}

// ---- queue tooling (D061) ----

/// An engine whose executor does not run, so entries stay as they are.
async fn queue_held(
    backend: &FakeBackend,
    dir: &std::path::Path,
) -> (Arc<Engine>, mpsc::UnboundedReceiver<Invalidation>, Ino) {
    let (engine, inval) = open(backend, dir).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let docs = found(engine.lookup(ROOT_INO, b"docs").expect("lookup")).ino;
    engine.ensure_listed(docs).await.expect("list");
    (engine, inval, docs)
}

/// A running engine that uploads only when forced.
async fn queue_forced_only(backend: &FakeBackend, dir: &std::path::Path) -> (Arc<Engine>, Ino) {
    let mut config = config();
    config.upload.debounce = HumanDuration(Duration::from_secs(3600));
    let (engine, _inval) = open_with(backend, dir, config).await;
    engine.start_background();
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let docs = found(engine.lookup(ROOT_INO, b"docs").expect("lookup")).ino;
    engine.ensure_listed(docs).await.expect("list");
    (engine, docs)
}

fn queue_path(path: &str) -> queue::QueueTarget {
    queue::QueueTarget::Path(path.to_owned())
}

async fn queue_all(engine: &Arc<Engine>) -> Vec<queue::QueueEntry> {
    engine
        .queue_entries(&queue::QueueTarget::All)
        .await
        .expect("entries")
}

#[tokio::test]
async fn queue_lists_each_kind_with_its_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval, docs) = queue_held(&backend, dir.path()).await;
    let top = found(engine.lookup(ROOT_INO, b"top.bin").expect("lookup")).ino;
    write_file(&engine, docs, "new.txt", b"new");
    engine.mkdir(ROOT_INO, b"made", 0o755).expect("mkdir");
    engine
        .rename(
            docs,
            b"a.txt",
            ROOT_INO,
            b"moved.txt",
            RenameMode::NoReplace,
        )
        .expect("rename");
    engine.unlink(ROOT_INO, b"top.bin").expect("unlink");
    let entries = queue_all(&engine).await;
    let kind_of = |path: &str| {
        entries
            .iter()
            .find(|entry| entry.path == path)
            .map(|entry| entry.kind)
    };
    assert_eq!(kind_of("/docs/new.txt"), Some(queue::QueueKind::Upload));
    assert_eq!(kind_of("/made"), Some(queue::QueueKind::CreateFolder));
    assert_eq!(kind_of("/moved.txt"), Some(queue::QueueKind::Move));
    // A deleted file is listed where its remote file is.
    assert_eq!(kind_of("/top.bin"), Some(queue::QueueKind::Trash));
    assert!(
        entries
            .iter()
            .all(|entry| entry.state == queue::QueueState::Waiting)
    );

    let under_docs = engine
        .queue_entries(&queue_path("/docs"))
        .await
        .expect("entries");
    let paths: Vec<&str> = under_docs.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["/docs/new.txt"]);
    let deleted = engine
        .queue_entries(&queue_path("/top.bin"))
        .await
        .expect("a deleted file is found by its old path");
    assert_eq!(deleted[0].id, top);
    assert!(deleted[0].base_rev.is_some());
    let by_id = engine
        .queue_entries(&queue::QueueTarget::Id(top))
        .await
        .expect("entries");
    assert_eq!(by_id.len(), 1);
    assert!(matches!(
        engine.queue_entries(&queue::QueueTarget::Id(999_999)).await,
        Err(CoreError::Refused(_))
    ));
}

#[tokio::test]
async fn queue_lists_metadata_and_shows_dependencies() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval, _docs) = queue_held(&backend, dir.path()).await;
    let top = found(engine.lookup(ROOT_INO, b"top.bin").expect("lookup")).ino;
    engine
        .setattr(
            top,
            &SetAttr {
                mode: Some(0o640),
                ..SetAttr::default()
            },
        )
        .expect("chmod");
    let made = engine.mkdir(ROOT_INO, b"made", 0o755).expect("mkdir").ino;
    write_file(&engine, made, "inner.txt", b"x");
    let entries = queue_all(&engine).await;
    let entry = |path: &str| {
        entries
            .iter()
            .find(|entry| entry.path == path)
            .expect("entry")
    };
    assert_eq!(entry("/top.bin").kind, queue::QueueKind::Metadata);
    assert_eq!(entry("/made/inner.txt").dependencies, ["/made"]);
    assert!(entry("/made").dependencies.is_empty());
}

#[tokio::test]
async fn queue_retry_uploads_a_parked_entry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    backend.inject_failure(
        pdmount_backend::fake::FakeOp::UploadNew,
        pdmount_backend::BackendError::QuotaExceeded,
    );
    let engine = running(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    write_file(&engine, ROOT_INO, "big", b"content");
    let counts = engine.drain(Duration::from_secs(10)).await;
    assert_eq!(counts.parked, 1);
    let entries = queue_all(&engine).await;
    assert!(matches!(entries[0].state, queue::QueueState::Parked { .. }));
    assert!(entries[0].last_error.is_some());

    let retried = engine
        .queue_retry(&queue_path("/big"))
        .await
        .expect("retry");
    assert_eq!(retried[0].path, "/big");
    settle(&engine).await;
    let root = backend.root().await.expect("root").link_id;
    let link = remote_child(&backend, &root, "big").await.link_id;
    assert_eq!(backend.content(&link).expect("uploaded"), &b"content"[..]);
}

#[tokio::test]
async fn queue_park_holds_an_entry_until_retry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _docs) = queue_forced_only(&backend, dir.path()).await;
    let ino = write_file(&engine, ROOT_INO, "held.txt", b"one");
    engine
        .queue_park(&queue::QueueTarget::Id(ino))
        .await
        .expect("park");
    // Neither a force nor a local change lets it go.
    overwrite(&engine, ino, b"two");
    let counts = engine.drain(Duration::from_secs(10)).await;
    assert_eq!((counts.pending, counts.parked), (0, 1));
    assert_eq!(engine.parked()[0].1, "parked by user");
    let root = backend.root().await.expect("root").link_id;
    let names: Vec<String> = backend
        .list_children(&root)
        .await
        .expect("list")
        .into_iter()
        .map(|node| node.name)
        .collect();
    assert!(!names.iter().any(|name| name == "held.txt"), "{names:?}");

    engine
        .queue_retry(&queue::QueueTarget::Id(ino))
        .await
        .expect("retry");
    settle(&engine).await;
    let link = remote_child(&backend, &root, "held.txt").await.link_id;
    assert_eq!(backend.content(&link).expect("uploaded"), &b"two"[..]);
}

#[tokio::test]
async fn queue_drop_of_a_modified_file_restores_the_remote_content() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, mut inval, docs) = queue_held(&backend, dir.path()).await;
    let ino = found(engine.lookup(docs, b"a.txt").expect("lookup")).ino;
    overwrite(&engine, ino, b"LOCAL");
    assert_eq!(read_file(&engine, ino).await, b"LOCAL");
    while inval.try_recv().is_ok() {}

    // Without --yes nothing happens.
    let preview = engine
        .queue_drop(&queue_path("/docs/a.txt"), false)
        .await
        .expect("preview");
    assert_eq!(preview.len(), 1);
    assert_eq!(read_file(&engine, ino).await, b"LOCAL");
    assert_eq!(queue_all(&engine).await.len(), 1);

    let dropped = engine
        .queue_drop(&queue_path("/docs/a.txt"), true)
        .await
        .expect("drop");
    assert_eq!(dropped[0].refused, None);
    assert_eq!(read_file(&engine, ino).await, b"hello world");
    assert_eq!(
        engine.xattr(ino, "user.pdmount.state").unwrap().as_deref(),
        Some("clean")
    );
    assert!(engine.staged_entry(ino).is_none());
    assert!(queue_all(&engine).await.is_empty());
    let mut invalidated = false;
    while let Ok(invalidation) = inval.try_recv() {
        invalidated |= matches!(invalidation, Invalidation::Inode { ino: seen } if seen == ino);
    }
    assert!(invalidated, "the kernel must forget the local content");
    // A later change works from the remote content.
    overwrite(&engine, ino, b"J");
    assert_eq!(read_file(&engine, ino).await, b"J");
}

#[tokio::test]
async fn queue_drop_of_a_modified_file_survives_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let ino = {
        let (engine, _inval, docs) = queue_held(&backend, dir.path()).await;
        let ino = found(engine.lookup(docs, b"a.txt").expect("lookup")).ino;
        overwrite(&engine, ino, b"LOCAL");
        engine
            .queue_drop(&queue::QueueTarget::Id(ino), true)
            .await
            .expect("drop");
        engine.shutdown().await.expect("shutdown");
        ino
    };
    let (engine, _inval) = open(&backend, dir.path()).await;
    assert_eq!(read_file(&engine, ino).await, b"hello world");
    assert!(queue_all(&engine).await.is_empty());
}

#[tokio::test]
async fn queue_drop_of_a_never_uploaded_file_removes_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, mut inval, _docs) = queue_held(&backend, dir.path()).await;
    let ino = write_file(&engine, ROOT_INO, "new.txt", b"new");
    while inval.try_recv().is_ok() {}
    engine
        .queue_drop(&queue_path("/new.txt"), true)
        .await
        .expect("drop");
    assert_eq!(engine.tree.read().child(ROOT_INO, "new.txt").unwrap(), None);
    assert!(queue_all(&engine).await.is_empty());
    let mut invalidated = false;
    while let Ok(invalidation) = inval.try_recv() {
        invalidated |= matches!(
            invalidation,
            Invalidation::Delete { child, .. } if child == ino
        ) || matches!(invalidation, Invalidation::Entry { name, .. } if name == "new.txt");
    }
    assert!(invalidated, "the kernel must forget the entry");
}

#[tokio::test]
async fn queue_drop_of_a_folder_undoes_its_subtree() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval, docs) = queue_held(&backend, dir.path()).await;
    let a = found(engine.lookup(docs, b"a.txt").expect("lookup")).ino;
    let made = engine.mkdir(ROOT_INO, b"made", 0o755).expect("mkdir").ino;
    let sub = engine.mkdir(made, b"sub", 0o755).expect("mkdir").ino;
    write_file(&engine, sub, "inner.txt", b"x");
    engine
        .rename(docs, b"a.txt", made, b"taken.txt", RenameMode::NoReplace)
        .expect("rename");
    write_file(&engine, ROOT_INO, "other.txt", b"stays");

    let dropped = engine
        .queue_drop(&queue_path("/made"), true)
        .await
        .expect("drop");
    assert!(
        dropped.iter().all(|item| item.refused.is_none()),
        "{dropped:?}"
    );
    let tree = engine.tree.read();
    assert_eq!(tree.child(ROOT_INO, "made").unwrap(), None);
    // The file moved into the folder is back in its remote place.
    assert_eq!(tree.child(docs, "a.txt").unwrap(), Some(a));
    assert!(!tree.placement_pending(a));
    drop(tree);
    assert_eq!(read_file(&engine, a).await, b"hello world");
    let left: Vec<String> = queue_all(&engine)
        .await
        .into_iter()
        .map(|e| e.path)
        .collect();
    assert_eq!(left, ["/other.txt"]);
}

#[tokio::test]
async fn queue_drop_of_a_delete_brings_the_remote_file_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval, _docs) = queue_held(&backend, dir.path()).await;
    engine.unlink(ROOT_INO, b"top.bin").expect("unlink");
    engine
        .queue_drop(&queue_path("/top.bin"), true)
        .await
        .expect("drop");
    let back = found(engine.lookup(ROOT_INO, b"top.bin").expect("lookup"));
    assert_eq!(read_file(&engine, back.ino).await, b"0123456789");
    assert!(queue_all(&engine).await.is_empty());
}

/// `mv a.txt b.txt; echo new > a.txt`: `a.txt` cannot go back while the new
/// file has its name, and can in one drop with it.
#[tokio::test]
async fn queue_drop_refuses_a_taken_name_and_settles_it_with_the_holder() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval, docs) = queue_held(&backend, dir.path()).await;
    let a = found(engine.lookup(docs, b"a.txt").expect("lookup")).ino;
    engine
        .rename(docs, b"a.txt", docs, b"b.txt", RenameMode::NoReplace)
        .expect("rename");
    write_file(&engine, docs, "a.txt", b"new");

    let refused = engine
        .queue_drop(&queue::QueueTarget::Id(a), true)
        .await
        .expect("drop");
    assert!(refused[0].refused.is_some(), "{refused:?}");
    // It stays queued, and the executor may take it again.
    assert_eq!(queue_all(&engine).await.len(), 2);
    assert!(!engine.journal.entries().is_empty());
    assert_eq!(engine.tree.read().child(docs, "b.txt").unwrap(), Some(a));

    let dropped = engine
        .queue_drop(&queue_path("/docs"), true)
        .await
        .expect("drop");
    assert!(
        dropped.iter().all(|item| item.refused.is_none()),
        "{dropped:?}"
    );
    assert_eq!(engine.tree.read().child(docs, "a.txt").unwrap(), Some(a));
    assert_eq!(engine.tree.read().child(docs, "b.txt").unwrap(), None);
    assert_eq!(read_file(&engine, a).await, b"hello world");
}

#[tokio::test]
async fn queue_drop_is_refused_while_the_upload_runs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _docs) = queue_forced_only(&backend, dir.path()).await;
    let ino = write_file(&engine, ROOT_INO, "new.txt", b"new");
    let pause = backend.pause_at(FakePoint::NewBeforeCommit);
    engine
        .queue_retry(&queue::QueueTarget::Id(ino))
        .await
        .expect("force");
    pause.reached().await;
    assert_eq!(
        queue_all(&engine).await[0].state,
        queue::QueueState::Running
    );
    assert!(matches!(
        engine.queue_drop(&queue::QueueTarget::All, true).await,
        Err(CoreError::Refused(_))
    ));
    assert_eq!(
        engine.tree.read().child(ROOT_INO, "new.txt").unwrap(),
        Some(ino)
    );
    pause.release();
    settle(&engine).await;
}

/// A step planned before the park or the drop must not start after it.
#[tokio::test]
async fn a_held_or_parked_entry_starts_no_step() {
    let journal = Journal::new(&config().upload);
    journal.touch(1);
    journal.touch(2);
    journal.park_by_user(&[1]);
    journal.hold(&[2]).expect("hold");
    assert!(!journal.start(1));
    assert!(!journal.start(2));
    assert!(journal.try_start_upload(1).is_none());
    assert!(journal.try_start_upload(2).is_none());
    journal.release(&[2]);
    assert!(journal.start(2));
    assert_eq!(journal.hold(&[1, 2]), Err(2));
}

#[tokio::test]
async fn sync_of_a_folder_returns_after_that_subtree_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs) = queue_forced_only(&backend, dir.path()).await;
    write_file(&engine, docs, "inside.txt", b"in");
    write_file(&engine, ROOT_INO, "outside.txt", b"out");
    let outcome = engine
        .sync_path(Some("/docs"), Duration::from_secs(10))
        .await
        .expect("sync");
    assert_eq!(outcome, queue::SyncOutcome::default());
    let root = backend.root().await.expect("root").link_id;
    let docs_link = remote_child(&backend, &root, "docs").await.link_id;
    let names = |nodes: Vec<RemoteNode>| -> Vec<String> {
        nodes.into_iter().map(|node| node.name).collect()
    };
    assert!(
        names(backend.list_children(&docs_link).await.expect("list"))
            .contains(&"inside.txt".to_owned())
    );
    assert!(
        !names(backend.list_children(&root).await.expect("list"))
            .contains(&"outside.txt".to_owned())
    );
    let left: Vec<String> = queue_all(&engine)
        .await
        .into_iter()
        .map(|e| e.path)
        .collect();
    assert_eq!(left, ["/outside.txt"]);

    // Without a path everything goes.
    let outcome = engine
        .sync_path(None, Duration::from_secs(10))
        .await
        .expect("sync");
    assert_eq!(outcome, queue::SyncOutcome::default());
    assert!(
        names(backend.list_children(&root).await.expect("list"))
            .contains(&"outside.txt".to_owned())
    );
}

/// The folder a synced file needs is forced too, and when it is parked the
/// sync says so at once instead of waiting for the timeout.
#[tokio::test]
async fn sync_reports_what_a_parked_entry_holds_up() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _docs) = queue_forced_only(&backend, dir.path()).await;
    let made = engine.mkdir(ROOT_INO, b"made", 0o755).expect("mkdir").ino;
    engine
        .queue_park(&queue::QueueTarget::Id(made))
        .await
        .expect("park");
    write_file(&engine, made, "inner.txt", b"x");
    let started = Instant::now();
    let outcome = engine
        .sync_path(Some("/made"), Duration::from_secs(60))
        .await
        .expect("sync");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(outcome.pending, 1);
    assert_eq!(
        outcome.parked,
        [("/made".to_owned(), "parked by user".to_owned())]
    );
    engine
        .queue_retry(&queue_path("/made"))
        .await
        .expect("retry");
    let outcome = engine
        .sync_path(Some("/made"), Duration::from_secs(10))
        .await
        .expect("sync");
    assert_eq!(outcome, queue::SyncOutcome::default());
}

#[tokio::test]
async fn sync_gives_up_at_its_timeout() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _docs) = queue_forced_only(&backend, dir.path()).await;
    write_file(&engine, ROOT_INO, "new.txt", b"new");
    let pause = backend.pause_at(FakePoint::NewBeforeCommit);
    let outcome = engine
        .sync_path(None, Duration::from_millis(300))
        .await
        .expect("sync");
    assert_eq!(outcome.pending, 1);
    pause.reached().await;
    pause.release();
    settle(&engine).await;
}

// ---- shutdown grace, read-only mounts, remote fsync paths (M4.12) ----

async fn remote_names(backend: &FakeBackend) -> Vec<String> {
    let root = backend.root().await.expect("root").link_id;
    backend
        .list_children(&root)
        .await
        .expect("list")
        .into_iter()
        .map(|node| node.name)
        .collect()
}

/// A running upload finishes within the grace; nothing new starts. What did
/// not start is still pending at the next mount.
#[tokio::test]
async fn shutdown_waits_for_a_running_upload_and_starts_nothing_new() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _docs) = queue_forced_only(&backend, dir.path()).await;
    let running = write_file(&engine, ROOT_INO, "running.txt", b"run");
    let pause = backend.pause_at(FakePoint::NewBeforeCommit);
    engine
        .queue_retry(&queue::QueueTarget::Id(running))
        .await
        .expect("force");
    pause.reached().await;
    let later = write_file(&engine, ROOT_INO, "later.txt", b"later");
    engine.stop_starting();
    engine.journal.force(later);
    let stopping = tokio::spawn({
        let engine = Arc::clone(&engine);
        async move { engine.shutdown().await }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!stopping.is_finished(), "it waits for the upload");
    pause.release();
    stopping.await.expect("join").expect("shutdown");
    let names = remote_names(&backend).await;
    assert!(names.contains(&"running.txt".to_owned()), "{names:?}");
    assert!(!names.contains(&"later.txt".to_owned()), "{names:?}");
    drop(engine);

    let (engine, _docs) = queue_forced_only(&backend, dir.path()).await;
    let left: Vec<String> = queue_all(&engine)
        .await
        .into_iter()
        .map(|e| e.path)
        .collect();
    assert_eq!(left, ["/later.txt"]);
    settle(&engine).await;
    assert!(
        remote_names(&backend)
            .await
            .contains(&"later.txt".to_owned())
    );
}

#[tokio::test]
async fn shutdown_gives_up_on_a_running_upload_after_the_grace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let mut config = config();
    config.upload.debounce = HumanDuration(Duration::from_secs(3600));
    config.timeouts.shutdown_grace = HumanDuration(Duration::from_millis(200));
    let (engine, _inval) = open_with(&backend, dir.path(), config).await;
    engine.start_background();
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let ino = write_file(&engine, ROOT_INO, "slow.txt", b"slow");
    let pause = backend.pause_at(FakePoint::NewBeforeCommit);
    engine.journal.force(ino);
    pause.reached().await;
    let started = Instant::now();
    engine.shutdown().await.expect("shutdown");
    assert!(started.elapsed() >= Duration::from_millis(200));
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(engine.queue_counts().in_flight, 1);
    assert!(
        !remote_names(&backend)
            .await
            .contains(&"slow.txt".to_owned())
    );
    pause.release();
}

#[tokio::test]
async fn a_read_only_engine_refuses_writes_and_uploads_what_is_pending() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (top, docs) = {
        let (engine, _inval, docs) = queue_held(&backend, dir.path()).await;
        write_file(&engine, ROOT_INO, "pending.txt", b"pending");
        let top = found(engine.lookup(ROOT_INO, b"top.bin").expect("lookup")).ino;
        engine.shutdown().await.expect("shutdown");
        (top, docs)
    };
    let mut options =
        EngineOptions::in_dir(dir.path(), config(), test_device("dev-a"), fixed_clock());
    options.read_only = true;
    let (engine, _inval) = Engine::open(Arc::new(backend.clone()), options)
        .await
        .expect("engine");
    engine.start_background();
    let ro = |result: Result<()>| assert!(matches!(result, Err(CoreError::ReadOnly)), "{result:?}");
    ro(engine
        .create(ROOT_INO, b"new", 0o644, true, WRITE)
        .map(|_| ()));
    ro(engine.mkdir(ROOT_INO, b"made", 0o755).map(|_| ()));
    ro(engine.unlink(ROOT_INO, b"top.bin"));
    ro(engine.rmdir(ROOT_INO, b"docs"));
    ro(engine.rename(ROOT_INO, b"top.bin", docs, b"t", RenameMode::Replace));
    ro(engine.open_file(top, WRITE).map(|_| ()));
    ro(engine.access(top, MAY_WRITE));
    ro(engine
        .setattr(
            top,
            &SetAttr {
                mode: Some(0o640),
                ..SetAttr::default()
            },
        )
        .map(|_| ()));
    assert_eq!(engine.getattr(top).expect("attr").perm & 0o222, 0);
    assert_eq!(read_file(&engine, top).await, b"0123456789");
    settle(&engine).await;
    assert!(
        remote_names(&backend)
            .await
            .contains(&"pending.txt".to_owned())
    );
}

#[tokio::test]
async fn remote_fsync_paths_select_by_the_path_from_the_mount_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let mut config = config();
    config.fsync.remote_paths = vec!["/docs/**".to_owned()];
    let (engine, _inval) = open_with(&backend, dir.path(), config).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let docs = found(engine.lookup(ROOT_INO, b"docs").expect("lookup")).ino;
    engine.ensure_listed(docs).await.expect("list");
    let inside = write_file(&engine, docs, "x", b"x");
    let outside = write_file(&engine, ROOT_INO, "y", b"y");
    assert!(engine.fsync_is_remote(inside));
    assert!(!engine.fsync_is_remote(outside));
    // The path at fsync time counts.
    engine
        .rename(docs, b"x", ROOT_INO, b"z", RenameMode::NoReplace)
        .expect("rename");
    assert!(!engine.fsync_is_remote(inside));
}

#[tokio::test]
async fn the_last_event_poll_is_known() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval, _docs) = queue_held(&backend, dir.path()).await;
    assert_eq!(engine.events_last_poll(), None);
    engine.sync_events().await.expect("events");
    assert!(engine.events_last_poll().is_some());
}

#[tokio::test]
async fn queue_drop_leaves_a_local_only_file_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval, _docs) = queue_held(&backend, dir.path()).await;
    let ino = write_file(&engine, ROOT_INO, "only-here.log", b"no other copy");
    engine.tree.write().get_mut(ino).expect("node").local_only = true;
    engine.journal.touch(ino);

    let dropped = engine
        .queue_drop(&queue::QueueTarget::All, true)
        .await
        .expect("drop");
    assert!(dropped.iter().all(|item| item.id != ino), "{dropped:?}");
    assert_eq!(read_file(&engine, ino).await, b"no other copy");
}

#[tokio::test]
async fn a_step_of_a_dropped_entry_does_not_start() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval, _docs) = queue_held(&backend, dir.path()).await;
    assert!(!engine.journal.start(9_999_999), "no entry, nothing to run");
}

/// A file of two staging blocks, staged with the first replaced, so the
/// second still comes from the base.
async fn partly_staged(backend: &FakeBackend, dir: &std::path::Path) -> (Arc<Engine>, Ino) {
    let root = backend.root().await.expect("root").link_id;
    backend.set_block_size(crate::staging::GRID as usize);
    backend
        .upload_new(
            &root,
            "big.bin",
            UploadSource::Bytes(Bytes::from(vec![b'r'; crate::staging::GRID as usize + 11])),
            meta(),
        )
        .await
        .expect("upload");
    let (engine, _inval, _docs) = queue_held(backend, dir).await;
    let ino = found(engine.lookup(ROOT_INO, b"big.bin").expect("lookup")).ino;
    let write = OpenMode {
        read: true,
        write: true,
        truncate: false,
    };
    let (handle, _) = engine.open_file(ino, write).expect("open");
    // A whole block: nothing is fetched, so nothing is read ahead either.
    engine
        .write_local(handle, 0, &vec![b'X'; crate::staging::GRID as usize])
        .expect("write");
    engine.release_file(handle);
    (engine, ino)
}

#[tokio::test]
async fn a_read_across_a_replaced_staging_reads_the_current_content() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, ino) = partly_staged(&backend, dir.path()).await;
    let (handle, _) = engine.open_file(ino, READ).expect("open");
    backend.set_latency(Duration::from_millis(500));
    // One byte of the staging file, and one of the base, still to fetch.
    let reader = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move {
            engine
                .read(handle, crate::staging::GRID - 1, 2)
                .await
                .map(ReadData::into_vec)
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    engine
        .queue_drop(&queue::QueueTarget::Id(ino), true)
        .await
        .expect("drop");
    overwrite(&engine, ino, b"Y");
    let read = reader.await.expect("reader");
    assert_eq!(read.expect("read"), b"", "the file is one byte long now");
}

#[tokio::test]
async fn a_late_materialize_does_not_write_into_a_newer_staging() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, ino) = partly_staged(&backend, dir.path()).await;
    backend.set_latency(Duration::from_millis(500));
    let late = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move { engine.materialize(ino, &[1], Priority::Interactive).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    engine
        .queue_drop(&queue::QueueTarget::Id(ino), true)
        .await
        .expect("drop");
    overwrite(&engine, ino, b"Y");
    late.await.expect("task").expect("materialize");
    let on_disk = std::fs::metadata(engine.staging.data_path(ino)).expect("staging");
    assert_eq!(
        on_disk.len(),
        1,
        "the old staging's block went into the new file"
    );
    backend.set_latency(Duration::ZERO);
    assert_eq!(read_file(&engine, ino).await, b"Y");
}

// ---- shutdown flush (D075) ----

/// The barrier's files upload at once, within an hour's debounce; a file
/// written after the barrier does not extend the wait and stays queued.
#[tokio::test]
async fn a_flush_uploads_the_barrier_without_the_debounce() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs) = queue_forced_only(&backend, dir.path()).await;
    write_file(&engine, docs, "one.txt", b"1");
    write_file(&engine, ROOT_INO, "two.txt", b"2");
    let barrier = engine.flush_barrier();
    assert_eq!(barrier.len(), 2);
    write_file(&engine, ROOT_INO, "later.txt", b"3");
    let outcome = engine
        .flush_to(&barrier, Instant::now() + Duration::from_secs(10))
        .await;
    assert_eq!(outcome.ended, crate::journal::FlushEnd::Reached);
    assert_eq!(
        (outcome.barrier, outcome.left, outcome.flushed()),
        (2, 0, 2)
    );
    let tree = fake_tree(&backend).await;
    assert_eq!(tree["/docs/one.txt"].content, b"1");
    assert_eq!(tree["/two.txt"].content, b"2");
    assert!(
        !tree.contains_key("/later.txt"),
        "a later write was waited for"
    );
    let left: Vec<String> = queue_all(&engine)
        .await
        .into_iter()
        .map(|e| e.path)
        .collect();
    assert_eq!(left, ["/later.txt"]);
}

/// Offline or in `needs_login` the flush does not wait (D075).
#[tokio::test]
async fn a_flush_does_not_wait_while_offline() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs) = queue_forced_only(&backend, dir.path()).await;
    write_file(&engine, docs, "one.txt", b"1");
    for state in [Connectivity::Offline, Connectivity::NeedsLogin] {
        engine.health().set(state);
        let barrier = engine.flush_barrier();
        let outcome = engine
            .flush_to(&barrier, Instant::now() + Duration::from_secs(10))
            .await;
        assert_eq!(outcome.ended, crate::journal::FlushEnd::NotOnline(state));
        assert_eq!(outcome.left, 1);
        assert!(outcome.elapsed < Duration::from_secs(1));
    }
}

/// The grace ends the wait: an upload slower than the deadline is left.
#[tokio::test]
async fn a_flush_ends_at_the_deadline() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs) = queue_forced_only(&backend, dir.path()).await;
    let pause = backend.pause_at(FakePoint::NewBeforeCommit);
    write_file(&engine, docs, "one.txt", b"1");
    let barrier = engine.flush_barrier();
    let outcome = engine
        .flush_to(&barrier, Instant::now() + Duration::from_millis(300))
        .await;
    assert_eq!(outcome.ended, crate::journal::FlushEnd::Deadline);
    assert_eq!(outcome.left, 1);
    assert!(outcome.elapsed < Duration::from_secs(1));
    pause.release();
    settle(&engine).await;
}

// ---- pending marker (D076) ----

/// A drive with `/docs` and blocks of the real size, so a marker is one
/// block.
async fn marker_drive() -> FakeBackend {
    let backend = FakeBackend::new();
    let root = backend.root().await.expect("root").link_id;
    backend
        .create_folder(&root, "docs", UNIX_EPOCH)
        .await
        .expect("mkdir");
    backend
}

fn marker_config() -> Config {
    let mut config = config();
    config.upload.debounce = HumanDuration(Duration::from_secs(3600));
    config.sync.pending_marker = true;
    config
}

/// The content of `/.pdmount/<device>.pending` on the drive.
async fn marker_on_drive(backend: &FakeBackend, device: &str) -> Option<Vec<u8>> {
    let name = marker::marker_name(&test_device(device).id);
    fake_tree(backend)
        .await
        .remove(&format!("/{}/{name}", marker::MARKER_DIR))
        .map(|file| file.content)
}

/// A marker another device left, uploaded as that device would.
async fn upload_marker(backend: &FakeBackend, device: &str, body: &[u8]) {
    let root = backend.root().await.expect("root").link_id;
    let existing = backend
        .list_children(&root)
        .await
        .expect("list")
        .into_iter()
        .find(|node| node.name == marker::MARKER_DIR);
    let folder = match existing {
        Some(node) => node.link_id,
        None => backend
            .create_folder(&root, marker::MARKER_DIR, UNIX_EPOCH)
            .await
            .expect("mkdir"),
    };
    backend
        .upload_new(
            &folder,
            &marker::marker_name(&test_device(device).id),
            UploadSource::Bytes(Bytes::copy_from_slice(body)),
            meta(),
        )
        .await
        .expect("upload");
}

async fn wait_for<F: std::future::Future<Output = bool>>(what: &str, mut check: impl FnMut() -> F) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Device A leaves a marker with what is queued, no file names; B reads it
/// through its event stream and announces it once; A's drain trashes it,
/// and B forgets it.
#[tokio::test]
async fn a_marker_is_written_read_by_another_device_and_trashed_after_the_drain() {
    let backend = marker_drive().await;
    let dir_b = tempfile::tempdir().expect("tempdir");
    let (b, _inval_b) = open_device(&backend, dir_b.path(), config(), "dev-b").await;
    b.gate.set_interval(Duration::ZERO);
    b.ensure_listed(ROOT_INO).await.expect("list");
    let mut notices = b.subscribe_incoming();

    let dir_a = tempfile::tempdir().expect("tempdir");
    let (a, _inval_a) = open_device(&backend, dir_a.path(), marker_config(), "dev-a").await;
    a.gate.set_interval(Duration::ZERO);
    a.start_background();
    a.ensure_listed(ROOT_INO).await.expect("list");
    let docs = found(a.lookup(ROOT_INO, b"docs").expect("lookup")).ino;
    a.ensure_listed(docs).await.expect("list");
    write_file(&a, docs, "secret-name.txt", b"0123456789");
    write_file(&a, ROOT_INO, "other.txt", b"abc");
    let deadline = Instant::now() + Duration::from_secs(5);
    assert_eq!(
        a.leave_pending_marker(deadline).await,
        marker::MarkerOutcome::Written {
            entries: 2,
            bytes: 13
        }
    );
    let body = marker_on_drive(&backend, "dev-a").await.expect("a marker");
    let text = String::from_utf8(body.clone()).expect("utf-8");
    assert!(!text.contains("secret-name"), "{text}");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(
        json,
        serde_json::json!({
            "version": 1, "device": "dev-a", "time": CLOCK_SECS, "entries": 2, "bytes": 13
        })
    );
    // A second unfinished stop gives it a new revision, not a second file.
    assert!(matches!(
        a.leave_pending_marker(deadline).await,
        marker::MarkerOutcome::Written { .. }
    ));
    let tree = fake_tree(&backend).await;
    assert_eq!(
        tree.keys()
            .filter(|path| path.starts_with("/.pdmount/"))
            .count(),
        1,
        "{tree:?}"
    );
    assert!(a.own_pending_marker().is_some());

    b.sync_events().await.expect("events");
    b.scan_markers().await.expect("scan");
    let incoming = b.incoming_markers();
    assert_eq!(incoming.len(), 1, "{incoming:?}");
    assert_eq!(incoming[0].device.as_deref(), Some("dev-a"));
    assert_eq!((incoming[0].entries, incoming[0].bytes), (2, 13));
    assert_eq!(
        notices.try_recv().expect("a notice").markers[0].device_id,
        incoming[0].device_id
    );
    b.scan_markers().await.expect("scan");
    assert!(notices.try_recv().is_err(), "announced twice");

    // The drain: the watcher trashes the marker.
    a.drain(Duration::from_secs(10)).await;
    wait_for("the marker's trash", || async {
        marker_on_drive(&backend, "dev-a").await.is_none()
    })
    .await;
    wait_for("the watcher", || async { a.own_pending_marker().is_none() }).await;
    b.sync_events().await.expect("events");
    b.scan_markers().await.expect("scan");
    assert!(b.incoming_markers().is_empty());
}

/// Nothing queued, no marker.
#[tokio::test]
async fn an_empty_queue_leaves_no_marker() {
    let backend = marker_drive().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let (engine, _inval) = open_with(&backend, dir.path(), marker_config()).await;
    assert_eq!(
        engine
            .leave_pending_marker(Instant::now() + Duration::from_secs(5))
            .await,
        marker::MarkerOutcome::Empty
    );
    assert!(!fake_tree(&backend).await.contains_key("/.pdmount"));
}

/// A marker from another device is untrusted: too large, malformed or of
/// an unknown version, it is ignored; its device name is cleaned.
#[tokio::test]
async fn markers_of_other_devices_are_bounded_and_cleaned() {
    let backend = marker_drive().await;
    upload_marker(&backend, "big", &vec![b' '; marker::MAX_MARKER_BYTES + 1]).await;
    upload_marker(&backend, "junk", b"{\"version\":1,\"entries\":\"x\"}").await;
    upload_marker(
        &backend,
        "v2",
        br#"{"version":2,"time":1,"entries":1,"bytes":1}"#,
    )
    .await;
    let evil = serde_json::json!({
        "version": 1, "device": "\u{1b}]0;title\u{7}evil\u{202e}", "time": 5, "entries": 4, "bytes": 9
    });
    upload_marker(&backend, "evil", &serde_json::to_vec(&evil).expect("json")).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let (engine, _inval) = open_device(&backend, dir.path(), config(), "dev-b").await;
    engine.gate.set_interval(Duration::ZERO);
    engine.ensure_listed(ROOT_INO).await.expect("list");
    engine.scan_markers().await.expect("scan");
    let incoming = engine.incoming_markers();
    assert_eq!(incoming.len(), 1, "{incoming:?}");
    assert_eq!(incoming[0].device.as_deref(), Some("?]0;title?evil?"));
    assert_eq!(incoming[0].entries, 4);
}

/// `.pdmount` is read-only in the mount (EROFS, D019): its name in the
/// root cannot be taken or removed, nothing in it changes, and its
/// attributes have no write bits. Reading works.
#[tokio::test]
async fn the_marker_folder_is_read_only() {
    let backend = marker_drive().await;
    upload_marker(
        &backend,
        "dev-x",
        &marker::encode(Some("x"), CLOCK_SECS, 1, 1),
    )
    .await;
    let dir = tempfile::tempdir().expect("tempdir");
    let (engine, _inval) = open(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let folder = found(engine.lookup(ROOT_INO, b".pdmount").expect("lookup"));
    assert_eq!(folder.perm & 0o222, 0);
    engine.ensure_listed(folder.ino).await.expect("list");
    let name = marker::marker_name(&test_device("dev-x").id);
    let file = found(engine.lookup(folder.ino, name.as_bytes()).expect("lookup"));
    assert_eq!(file.perm & 0o222, 0);
    assert_eq!(engine.getattr(file.ino).expect("attr").perm & 0o222, 0);
    assert!(!read_file(&engine, file.ino).await.is_empty());
    assert_eq!(
        engine
            .xattr(file.ino, surface::XATTR_ACCESS)
            .expect("xattr")
            .as_deref(),
        Some("ro")
    );

    let ro = |result: Result<()>| assert!(matches!(result, Err(CoreError::ReadOnly)), "{result:?}");
    let chmod = |mode| SetAttr {
        mode: Some(mode),
        ..SetAttr::default()
    };
    ro(engine.rmdir(ROOT_INO, b".pdmount"));
    ro(engine.unlink(folder.ino, name.as_bytes()));
    ro(engine
        .create(folder.ino, b"new", 0o644, true, WRITE)
        .map(drop));
    ro(engine.mkdir(folder.ino, b"d", 0o755).map(drop));
    ro(engine.open_file(file.ino, WRITE).map(drop));
    ro(engine.setattr(file.ino, &chmod(0o644)).map(drop));
    ro(engine.setattr(folder.ino, &chmod(0o777)).map(drop));
    ro(engine.access(file.ino, MAY_WRITE));
    let rename = |from: Ino, name: &[u8], to: Ino, new_name: &[u8]| {
        engine.rename(from, name, to, new_name, RenameMode::Replace)
    };
    ro(rename(ROOT_INO, b".pdmount", ROOT_INO, b"moved"));
    ro(rename(folder.ino, name.as_bytes(), ROOT_INO, b"out"));
    let top = write_file(&engine, ROOT_INO, "top.txt", b"t");
    ro(rename(ROOT_INO, b"top.txt", folder.ino, b"in"));
    ro(rename(ROOT_INO, b"top.txt", ROOT_INO, b".pdmount"));
    assert!(engine.getattr(top).is_ok());
    engine.access(file.ino, MAY_READ).expect("readable");
}

/// The name is taken in the root even before the folder exists, and only
/// in the root.
#[tokio::test]
async fn the_marker_folder_name_cannot_be_created_in_the_root() {
    let backend = marker_drive().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let (engine, _inval) = open(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    assert!(matches!(
        engine.mkdir(ROOT_INO, b".pdmount", 0o755),
        Err(CoreError::ReadOnly)
    ));
    assert!(matches!(
        engine.create(ROOT_INO, b".pdmount", 0o644, true, WRITE),
        Err(CoreError::ReadOnly)
    ));
    let docs = found(engine.lookup(ROOT_INO, b"docs").expect("lookup")).ino;
    engine.ensure_listed(docs).await.expect("list");
    engine
        .mkdir(docs, b".pdmount", 0o755)
        .expect("elsewhere it is a name");
}

/// A marker that another client trashed, or that the store remembers after
/// it was trashed, is forgotten at the drain without a second trash.
#[tokio::test]
async fn a_marker_trashed_elsewhere_is_forgotten_at_the_drain() {
    let backend = marker_drive().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let (engine, _inval) = open_with(&backend, dir.path(), marker_config()).await;
    engine.gate.set_interval(Duration::ZERO);
    engine.ensure_listed(ROOT_INO).await.expect("list");
    write_file(&engine, ROOT_INO, "f.txt", b"f");
    assert!(matches!(
        engine
            .leave_pending_marker(Instant::now() + Duration::from_secs(5))
            .await,
        marker::MarkerOutcome::Written { .. }
    ));
    let link = engine.own_pending_marker().expect("recorded");
    backend
        .trash(std::slice::from_ref(&link))
        .await
        .expect("trash")
        .remove(0)
        .expect("trashed");
    engine.start_background();
    settle(&engine).await;
    wait_for("the watcher", || async {
        engine.own_pending_marker().is_none()
    })
    .await;
    assert!(marker_on_drive(&backend, "dev-a").await.is_none());
}

/// A flush that left a marker and then reached its barrier with nothing
/// left trashes the marker itself, before the stop ends the drain watcher
/// (D096); with entries left it keeps it.
#[tokio::test]
async fn the_end_of_a_flush_trashes_a_marker_it_no_longer_needs() {
    let backend = marker_drive().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let (engine, _inval) = open_with(&backend, dir.path(), marker_config()).await;
    engine.gate.set_interval(Duration::ZERO);
    engine.ensure_listed(ROOT_INO).await.expect("list");
    write_file(&engine, ROOT_INO, "f.txt", b"f");
    let deadline = Instant::now() + Duration::from_secs(10);
    assert!(matches!(
        engine.leave_pending_marker(deadline).await,
        marker::MarkerOutcome::Written { .. }
    ));
    assert!(
        !engine.trash_pending_marker_by(deadline).await,
        "the queue is not empty"
    );
    assert!(marker_on_drive(&backend, "dev-a").await.is_some());

    // No drain watcher: only the journal runs, as at the end of a stop.
    start_journal(&engine);
    let barrier = engine.flush_barrier();
    let outcome = engine.flush_to(&barrier, deadline).await;
    assert_eq!(
        outcome.ended,
        crate::journal::FlushEnd::Reached,
        "{outcome:?}"
    );
    assert!(engine.trash_pending_marker_by(deadline).await);
    assert!(engine.own_pending_marker().is_none());
    assert!(marker_on_drive(&backend, "dev-a").await.is_none());
    // Nothing left to trash.
    assert!(engine.trash_pending_marker_by(deadline).await);
}

/// A marker announced before a restart is not announced again when its
/// first fetch after the restart fails once.
#[tokio::test]
async fn a_failed_fetch_after_a_restart_does_not_announce_a_marker_again() {
    let backend = marker_drive().await;
    upload_marker(
        &backend,
        "dev-x",
        &marker::encode(Some("x"), CLOCK_SECS, 1, 1),
    )
    .await;
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let (engine, _inval) = open_device(&backend, dir.path(), config(), "dev-b").await;
        engine.gate.set_interval(Duration::ZERO);
        let mut notices = engine.subscribe_incoming();
        engine.ensure_listed(ROOT_INO).await.expect("list");
        engine.scan_markers().await.expect("scan");
        assert!(notices.try_recv().is_ok(), "announced");
        engine.shutdown().await.expect("shutdown");
    }
    let (engine, _inval) = open_device(&backend, dir.path(), config(), "dev-b").await;
    engine.gate.set_interval(Duration::ZERO);
    let mut notices = engine.subscribe_incoming();
    engine.ensure_listed(ROOT_INO).await.expect("list");
    backend.inject_failure(
        pdmount_backend::fake::FakeOp::Blocks,
        BackendError::Transient("once".into()),
    );
    engine.scan_markers().await.expect("scan");
    assert!(engine.incoming_markers().is_empty(), "not read yet");
    engine.scan_markers().await.expect("scan");
    assert_eq!(engine.incoming_markers().len(), 1);
    assert!(notices.try_recv().is_err(), "announced again");
}

/// D058, D110: the trees of a session share its upload limit and its
/// background gate: two trees that each write several files upload at most
/// `upload.concurrency` of them at once together.
#[tokio::test]
async fn trees_sharing_the_session_limits_upload_concurrency_files_at_most() {
    let backend = FakeBackend::new();
    backend.set_block_size(4);
    backend.set_profile(pdmount_backend::fake::NetProfile {
        rtt: Duration::from_millis(50),
        up_bytes_per_sec: None,
        down_bytes_per_sec: None,
    });
    let mut config = config();
    config.upload.concurrency = 2;
    let limits = crate::limits::SessionLimits::new(&config);
    let dir = tempfile::tempdir().expect("tempdir");
    let mut engines = Vec::new();
    for name in ["One", "Two"] {
        let root = backend.add_root(name, None, None).expect("root");
        let mut options = EngineOptions::in_dir(
            &dir.path().join(name),
            config.clone(),
            test_device("dev-a"),
            fixed_clock(),
        );
        options.limits = Some(Arc::clone(&limits));
        let (engine, _inval) = Engine::open(
            Arc::new(backend.tree(&root.volume, root.root.clone())),
            options,
        )
        .await
        .expect("engine");
        engine.ensure_listed(ROOT_INO).await.expect("list");
        engine.reserve_inos(16).await.expect("inos");
        engines.push(engine);
    }
    assert!(Arc::ptr_eq(&engines[0].gate, &engines[1].gate));
    backend.take_peak_uploads();
    for engine in &engines {
        for index in 0..6 {
            write_file(engine, ROOT_INO, &format!("f{index}"), b"0123456789");
        }
    }
    for engine in &engines {
        engine.start_background();
    }
    for engine in &engines {
        settle(engine).await;
    }
    assert_eq!(backend.take_peak_uploads(), 2, "two trees, one session");
    for engine in &engines {
        engine.shutdown().await.expect("shutdown");
    }
}

/// D114 (review): once a tree is retired (a gone share), the reply of a
/// change whose commit ends later answers EIO, so a write is never
/// acknowledged and then left out of the export; the write gate is closed.
/// A change that committed before is acknowledged.
#[tokio::test]
async fn a_retired_engine_acknowledges_no_change_after_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval) = open(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let reply = |engine: &Engine| {
        let (sender, receiver) = std::sync::mpsc::channel();
        engine.after_commit_kept(move |kept| {
            let _ = sender.send(kept);
        });
        receiver
    };
    let (attr, handle) = engine
        .create(ROOT_INO, b"kept.txt", 0o644, true, WRITE)
        .expect("create");
    engine.write_local(handle, 0, b"before").expect("write");
    let before = reply(&engine);
    engine.store.flush().await.expect("flush");
    assert!(before.recv().expect("reply").is_ok());

    // A write in flight when the tree is retired: queued, then retired,
    // then committed (the writer thread waits for `release` first).
    let (release, wait) = std::sync::mpsc::channel::<()>();
    engine.store.write(move |_| {
        let _ = wait.recv();
        Ok(())
    });
    engine.write_local(handle, 6, b" in flight").expect("write");
    let in_flight = reply(&engine);
    engine.retire();
    release.send(()).expect("release");
    engine.store.flush().await.expect("flush");
    assert!(matches!(
        in_flight.recv().expect("reply"),
        Err(CoreError::Errno(nix::libc::EIO))
    ));
    assert!(engine.is_retired() && !engine.write_gate_open());
    assert!(matches!(
        engine.write_local(handle, 0, b"after"),
        Err(CoreError::ReadOnly)
    ));
    engine.release_file(handle);
    // Its content stays queued for the export.
    assert!(engine.queue_counts().pending > 0);
    assert_eq!(read_file(&engine, attr.ino).await, b"before in flight");
    engine.shutdown().await.expect("shutdown");
}

mod brought_back;
mod lifecycle;
mod markers;
mod scan;
mod swap;
