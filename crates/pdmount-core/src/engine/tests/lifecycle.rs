//! Background work that must end or go on (D120): the refresh task of a
//! tree that is gone or stopped, and the event sync under a steady load.

use super::*;
use pdmount_backend::fake::{FakeRole, NetProfile};

/// A refresh of a tree whose root was revoked ends, instead of listing it
/// again every 30 s and keeping the engine alive; it stays owed, so no
/// cursor is stored for the tree.
#[tokio::test]
async fn the_refresh_of_a_revoked_tree_ends() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeBackend::new();
    let added = fake
        .add_root("Team", Some("owner@example.test"), Some(FakeRole::Viewer))
        .expect("share");
    let share = added.share.clone().expect("share id");
    let tree = fake.tree(&added.volume, added.root.clone());
    let mut options =
        EngineOptions::in_dir(dir.path(), config(), test_device("dev-a"), fixed_clock());
    options.cache.disk_bytes = 1 << 20;
    let (engine, _inval) = Engine::open(Arc::new(tree), options).await.expect("engine");
    engine.gate.set_interval(Duration::ZERO);
    engine.ensure_listed(ROOT_INO).await.expect("list");
    eventually("the listing's tasks", || Arc::strong_count(&engine) == 1).await;

    fake.set_own_role(&share, None).expect("revoke");
    {
        let _cursor = engine.event_cursor.lock().await;
        engine.begin_refresh();
    }
    eventually("the end of the refresh task", || {
        Arc::strong_count(&engine) == 1
    })
    .await;
    assert!(
        engine.events_refreshing(),
        "still owed: no cursor is stored"
    );
}

/// A shutdown ends a refresh that waits to list again, so a stopped
/// engine keeps no task of its own; a refresh asked for later starts none.
#[tokio::test]
async fn a_shutdown_ends_the_refresh_task() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval) = open(&backend, dir.path()).await;
    engine.gate.set_interval(Duration::ZERO);
    engine.ensure_listed(ROOT_INO).await.expect("list");
    eventually("the listing's tasks", || Arc::strong_count(&engine) == 1).await;

    // The root's listing fails once: the refresh waits 30 s to list again.
    backend.inject_failure(
        pdmount_backend::fake::FakeOp::ListChildren,
        BackendError::Other("injected".into()),
    );
    {
        let _cursor = engine.event_cursor.lock().await;
        engine.begin_refresh();
    }
    eventually("the failed listing", || listings(&engine) >= 2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(Arc::strong_count(&engine) > 1, "the refresh task runs");
    engine.shutdown().await.expect("shutdown");
    eventually("no task left", || Arc::strong_count(&engine) == 1).await;
    {
        let _cursor = engine.event_cursor.lock().await;
        engine.begin_refresh();
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(Arc::strong_count(&engine), 1, "no refresh after a stop");
}

/// A steady stream of requests a user waits on in one tree, with a pin
/// download running there, does not stop another tree's event sync: its
/// turns wait for the users at most their patience, and the pin's blocks
/// hold no turn.
#[tokio::test(flavor = "multi_thread")]
async fn another_trees_events_arrive_under_steady_use_and_a_pin_download() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeBackend::new();
    fake.set_block_size(4096);
    let main_root = fake.root().await.expect("root").link_id;
    // 128 blocks: 32 s to download at 16 KiB/s.
    let big: Vec<u8> = (0..128 * 4096).map(|i| (i % 251) as u8).collect();
    fake.upload_new(
        &main_root,
        "big.bin",
        UploadSource::Bytes(Bytes::from(big)),
        meta(),
    )
    .await
    .expect("upload");
    let device = fake.add_root("Laptop", None, None).expect("device");
    fake.set_profile(NetProfile {
        rtt: Duration::ZERO,
        up_bytes_per_sec: None,
        down_bytes_per_sec: Some(16 << 10),
    });
    let config = config();
    let limits = crate::limits::SessionLimits::new(&config);
    let open_tree = |backend: FakeBackend, name: &str| {
        let mut options = EngineOptions::in_dir(
            &dir.path().join(name),
            config.clone(),
            test_device("dev-a"),
            fixed_clock(),
        );
        options.cache.disk_bytes = 1 << 20;
        options.limits = Some(Arc::clone(&limits));
        async move {
            Engine::open(Arc::new(backend), options)
                .await
                .expect("engine")
                .0
        }
    };
    let my_files = open_tree(fake.clone(), "my-files").await;
    let laptop = open_tree(fake.tree(&device.volume, device.root.clone()), "laptop").await;
    my_files.ensure_listed(ROOT_INO).await.expect("list");
    laptop.ensure_listed(ROOT_INO).await.expect("list");
    my_files.runtime().spawn(Arc::clone(&my_files).run_pins());
    my_files.pin_set("/big.bin").await.expect("pin");
    eventually("the pin's download", || {
        my_files.cache_stats().pinned_bytes > 0
    })
    .await;

    // From now on a request a user waits on is always in flight.
    let steady = my_files.user_wait();
    fake.upload_new(
        &device.root,
        "new.txt",
        UploadSource::Bytes(Bytes::from_static(b"new")),
        meta(),
    )
    .await
    .expect("upload");
    let synced = tokio::time::timeout(Duration::from_secs(30), laptop.sync_events()).await;
    synced
        .expect("the event sync waited for the other tree's users")
        .expect("sync");
    found(laptop.lookup(ROOT_INO, b"new.txt").expect("lookup"));
    let pins = my_files.pin_list().await.expect("pins");
    assert!(
        pins[0].fetched_bytes < pins[0].total_bytes,
        "the pin download still runs: {:?}",
        pins[0]
    );
    drop(steady);
}

/// A pin's block downloads hold no background turn: the session's other
/// background work takes its turns while a large pinned file downloads
/// (before, the pin held one turn per 16 blocks).
#[tokio::test(flavor = "multi_thread")]
async fn a_pin_download_holds_no_background_turn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeBackend::new();
    fake.set_block_size(4096);
    let root = fake.root().await.expect("root").link_id;
    // 64 blocks: 16 of them take 8 s at 8 KiB/s.
    let big: Vec<u8> = (0..64 * 4096).map(|i| (i % 251) as u8).collect();
    fake.upload_new(
        &root,
        "big.bin",
        UploadSource::Bytes(Bytes::from(big)),
        meta(),
    )
    .await
    .expect("upload");
    let (engine, _inval) = open(&fake, dir.path()).await;
    engine.gate.set_interval(Duration::ZERO);
    engine.ensure_listed(ROOT_INO).await.expect("list");
    fake.set_profile(NetProfile {
        rtt: Duration::ZERO,
        up_bytes_per_sec: None,
        down_bytes_per_sec: Some(8 << 10),
    });
    engine.runtime().spawn(Arc::clone(&engine).run_pins());
    engine.pin_set("/big.bin").await.expect("pin");
    eventually("the pin's download", || {
        engine.cache_stats().pinned_bytes > 0
    })
    .await;

    let turn = tokio::time::timeout(Duration::from_secs(3), engine.gate.background()).await;
    assert!(turn.is_ok(), "the pin's blocks held the background turn");
    drop(turn);
    let pins = engine.pin_list().await.expect("pins");
    assert!(
        pins[0].fetched_bytes < pins[0].total_bytes,
        "the pin download still runs: {:?}",
        pins[0]
    );
}
