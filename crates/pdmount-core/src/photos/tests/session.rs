//! The Photos tree on the session's limits and bulk-read guard, beside an
//! engine tree (D027, D110, D111).

use super::*;
use crate::device::{DeviceIdentity, Uuid};
use crate::engine::{Engine, EngineOptions, OpenMode};
use crate::units::{ByteSize, HumanDuration};
use pdmount_backend::FileMeta;

const READ: OpenMode = OpenMode {
    read: true,
    write: false,
    truncate: false,
};

/// An engine over the fake's My Files with `files` files of 64 bytes in
/// its root, listed, and their inode numbers.
async fn engine_with(
    fake: &FakeBackend,
    dir: &std::path::Path,
    config: Config,
    files: usize,
) -> (Arc<Engine>, Vec<Ino>) {
    let root = fake.root().await.expect("root").link_id;
    for n in 0..files {
        fake.upload_new(
            &root,
            &format!("f{n}"),
            UploadSource::Bytes(Bytes::from(vec![n as u8; 64])),
            FileMeta {
                mtime: UNIX_EPOCH,
                mode: Some(0o600),
                media_type: "application/octet-stream".into(),
                sha1: None,
            },
        )
        .await
        .expect("upload");
    }
    let device = DeviceIdentity {
        id: Uuid::from_bytes([1; 16]),
        name: "dev-a".into(),
    };
    let clock: crate::conflicts::Clock = Arc::new(SystemTime::now);
    let options = EngineOptions::in_dir(dir, config, device, clock);
    let (engine, _invalidations) = Engine::open(Arc::new(fake.clone()), options)
        .await
        .expect("engine");
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let inos = (0..files)
        .map(
            |n| match engine.lookup(ROOT_INO, format!("f{n}").as_bytes()) {
                Ok(Lookup::Found(attr)) => attr.ino,
                other => panic!("f{n}: {other:?}"),
            },
        )
        .collect();
    (engine, inos)
}

/// The Photos tree on `engine`'s session limits and guard, its timeline
/// read and the names of March 2023 fetched; the month's inode.
async fn photos_beside(
    fake: &FakeBackend,
    dir: &std::path::Path,
    engine: &Engine,
    config: Config,
) -> (Setup, Ino) {
    let setup = photos_tree_beside(fake, dir, engine, config).await;
    let tree = &setup.tree;
    tree.read_timeline().await.expect("timeline");
    fetch_all(tree, (2023, 3)).await;
    let year = ino_of(tree, ROOT_INO, "2023");
    let month = ino_of(tree, year, "03");
    (setup, month)
}

/// The Photos tree on `engine`'s session limits and guard, nothing read.
async fn photos_tree_beside(
    fake: &FakeBackend,
    dir: &std::path::Path,
    engine: &Engine,
    config: Config,
) -> Setup {
    let tree_backend = fake.photos_tree().expect("photos tree");
    let volume = fake.photos_volume_now().expect("a Photos volume");
    let options = PhotosOptions {
        volume: volume.volume,
        root: volume.root,
        state_db: dir.join("state.db"),
        cache: CacheOptions {
            dir: dir.join("blocks"),
            master_key: Some([7; 32]),
            ram_bytes: 16 << 20,
            disk_bytes: 64 << 20,
            shared: None,
            downloads: None,
        },
        config,
        health: None,
        placer: utc_month,
        thumbnails: None,
        scan: Some(Arc::clone(engine.scan_guard())),
        limits: Some(Arc::clone(engine.limits())),
    };
    let (tree, invalidations) =
        PhotosTree::open(Arc::new(tree_backend), Arc::new(fake.clone()), options)
            .await
            .expect("open");
    tree.gate.set_interval(Duration::ZERO);
    Setup {
        fake: fake.clone(),
        tree,
        _invalidations: invalidations,
    }
}

/// What the FUSE layer does with a request: asks again after each
/// `resolve` until it is served, within `deadline`.
async fn served<T>(
    tree: &Arc<PhotosTree>,
    deadline: Duration,
    attempt: impl Fn(&PhotosTree) -> Result<T>,
) -> T {
    tokio::time::timeout(deadline, async {
        loop {
            match attempt(tree) {
                Err(error @ CoreError::NeedsListing(_)) => {
                    tree.resolve(&error).await.expect("resolve");
                }
                other => return other.expect("served"),
            }
        }
    })
    .await
    .expect("served within the deadline")
}

/// Finding 3: requests a user waits on in Photos are served while a user's
/// request in another tree holds the session's gate the whole time (a long
/// `cp` in My Files): the first listing of Photos, a month's first names,
/// and a lookup of a name in a later batch, well within the 20 s metadata
/// deadline (D119). Before, each waited for a background turn that never
/// came, and answered EIO at the deadline.
#[tokio::test(flavor = "multi_thread")]
async fn photos_are_served_while_another_tree_holds_the_gate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeBackend::new();
    add(&fake, "IMG_0.jpg", MARCH_2023).await;
    for n in 0..NAME_BATCH + 5 {
        add(&fake, &format!("J{n:03}.jpg"), JULY_2023 + n as i64).await;
    }
    let (engine, _) = engine_with(&fake, &dir.path().join("my-files"), Config::default(), 0).await;
    let busy = engine.user_wait();
    let setup = photos_tree_beside(
        &fake,
        &dir.path().join("photos"),
        &engine,
        Config::default(),
    )
    .await;
    let tree = &setup.tree;
    tree.start();
    let deadline = Duration::from_secs(10);

    let (handle, _) = served(tree, deadline, |tree| tree.opendir(ROOT_INO)).await;
    let years: Vec<String> = tree
        .dir_snapshot(handle)
        .expect("snapshot")
        .iter()
        .map(|entry| entry.name.clone())
        .collect();
    tree.releasedir(handle);
    assert_eq!(years, [".", "..", "2023"]);
    let july = served(tree, deadline, |tree| {
        let year = match tree.lookup(ROOT_INO, b"2023")? {
            Lookup::Found(attr) => attr.ino,
            Lookup::Negative => return Err(CoreError::NotFound),
        };
        tree.lookup(year, b"07")
    })
    .await;
    let Lookup::Found(july) = july else {
        panic!("no July");
    };
    // A month without names: its first batch.
    let (handle, _) = served(tree, deadline, |tree| tree.opendir(july.ino)).await;
    assert_eq!(
        tree.dir_snapshot(handle).expect("snapshot").len(),
        NAME_BATCH + 2
    );
    tree.releasedir(handle);
    // The oldest photo of the month is in its second batch.
    let found = served(tree, deadline, |tree| tree.lookup(july.ino, b"J000.jpg")).await;
    assert!(matches!(found, Lookup::Found(_)), "{found:?}");
    drop(busy);
    tree.shutdown().await.expect("shutdown");
}

/// One gate for the session: a request a user waits on in the engine tree
/// holds back the Photos tree's background turns, and a Photos read holds
/// back the engine's (D027, D110).
#[tokio::test(flavor = "multi_thread")]
async fn photos_and_an_engine_tree_take_turns_of_one_gate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeBackend::new();
    add(&fake, "IMG_0.jpg", MARCH_2023).await;
    let (engine, _) = engine_with(&fake, &dir.path().join("my-files"), Config::default(), 0).await;
    let (setup, month) = photos_beside(
        &fake,
        &dir.path().join("photos"),
        &engine,
        Config::default(),
    )
    .await;
    let tree = &setup.tree;
    assert!(Arc::ptr_eq(&tree.gate, &engine.gate), "the session's gate");

    // A user's request in the engine tree: the timeline waits for it.
    let waiting = engine.user_wait();
    let timeline = tokio::spawn({
        let tree = Arc::clone(tree);
        async move { tree.read_timeline().await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !timeline.is_finished(),
        "a Photos background turn went ahead of a user's request in another tree"
    );
    drop(waiting);
    tokio::time::timeout(Duration::from_secs(10), timeline)
        .await
        .expect("the timeline read goes on once the request ended")
        .expect("join")
        .expect("timeline");

    // A Photos read a user waits on (slow: 400 ms a call): the engine's
    // background turn waits for it.
    // Its two calls (block list, block) take 800 ms; an engine background
    // turn asked for 100 ms in starts only after them.
    let photo = ino_of(tree, month, "IMG_0.jpg");
    let (handle, _) = tree.open(photo).expect("open");
    fake.set_latency(Duration::from_millis(400));
    let started = Instant::now();
    let read = tokio::spawn({
        let tree = Arc::clone(tree);
        async move { tree.read(handle, 0, 100).await.map(ReadData::into_vec) }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let turn = tokio::spawn({
        let engine = Arc::clone(&engine);
        async move {
            let _turn = engine.gate.background().await;
            started.elapsed()
        }
    });
    let data = read.await.expect("join").expect("read");
    assert_eq!(data, b"content of IMG_0.jpg");
    let turn_after = turn.await.expect("join");
    assert!(
        turn_after >= Duration::from_millis(750),
        "the engine's background turn started {turn_after:?} in, during a Photos read"
    );
    fake.set_latency(Duration::ZERO);
    tree.release(handle);
}

/// One guard for the session: opens in an engine tree and in Photos count
/// together, so a burst of Photos opens begins a scan; the photos opened
/// during it are read as scan reads, paced over the session, held back by a
/// user's request in the engine tree, and their blocks count against the
/// scan share (D081, D111).
#[tokio::test(flavor = "multi_thread")]
async fn a_burst_of_photo_opens_is_a_scan_of_the_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeBackend::new();
    for n in 0..7 {
        add(&fake, &format!("IMG_{n}.jpg"), MARCH_2023 + i64::from(n)).await;
    }
    let mut config = Config::default();
    config.scan.opens = 4;
    config.scan.interval = HumanDuration(Duration::from_millis(200));
    // Less than a photo: a second read through a handle is interactive.
    config.scan.first_bytes = ByteSize(10);
    let (engine, files) = engine_with(&fake, &dir.path().join("my-files"), config.clone(), 2).await;
    let (setup, month) = photos_beside(&fake, &dir.path().join("photos"), &engine, config).await;
    let tree = &setup.tree;
    let guard = engine.scan_guard();
    let scans = |tree: &PhotosTree| tree.metrics.scans.load(Ordering::Relaxed);
    let scan_reads = |tree: &PhotosTree| tree.metrics.scan_reads.load(Ordering::Relaxed);

    // Two files of My Files and two photos: the fourth file begins it.
    for &ino in &files {
        engine.open_file(ino, READ).expect("open");
    }
    let photos: Vec<Ino> = (0..7)
        .map(|n| ino_of(tree, month, &format!("IMG_{n}.jpg")))
        .collect();
    let handles: Vec<u64> = photos
        .iter()
        .map(|&ino| tree.open(ino).expect("open").0)
        .collect();
    assert!(guard.scanning());
    assert_eq!(scans(tree), 1, "the Photos open began the scan");
    let classed: Vec<bool> = handles
        .iter()
        .map(|&handle| tree.file(handle).expect("handle").scan)
        .collect();
    assert_eq!(classed, [false, true, true, true, true, true, true]);

    // A user's request in the engine tree holds a scan read back.
    let waiting = engine.user_wait();
    let held = tokio::spawn({
        let tree = Arc::clone(tree);
        let handle = handles[1];
        async move { tree.read(handle, 0, 100).await.map(drop) }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !held.is_finished(),
        "a Photos scan read went ahead of a user's request in another tree"
    );
    drop(waiting);
    held.await.expect("join").expect("read");

    // Five more scan reads at once: their downloads start 200 ms apart.
    let started = std::time::Instant::now();
    let reads: Vec<_> = handles[2..]
        .iter()
        .map(|&handle| {
            let tree = Arc::clone(tree);
            tokio::spawn(async move { tree.read(handle, 0, 100).await.map(drop) })
        })
        .collect();
    for read in reads {
        read.await.expect("join").expect("read");
    }
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(4 * 200),
        "5 scan downloads in {took:?}"
    );
    assert_eq!(scan_reads(tree), 6);
    assert!(
        guard.held() > 0,
        "the Photos tree's scan blocks are tracked"
    );

    // The photo opened before the scan reads interactively.
    tree.read(handles[0], 0, 100).await.expect("read");
    assert_eq!(scan_reads(tree), 6, "an interactive handle");
    // Past `scan.first_bytes` a scan handle reads interactively too, and
    // the scan block it uses leaves the scan blocks.
    let before = guard.held();
    let data = tree.read(handles[1], 0, 100).await.expect("read");
    assert_eq!(data.into_vec(), b"content of IMG_1.jpg");
    assert_eq!(scan_reads(tree), 6, "past the first bytes");
    assert!(guard.held() < before, "a block in use is no scan block");
    for handle in handles {
        tree.release(handle);
    }
}
