//! A clean file removed remotely while a handle is open on it, then written
//! through that handle (D128): the file comes back with the removed folders
//! above it, and the content reaches the drive (§10.5).

use super::*;

const READ_WRITE: OpenMode = OpenMode {
    read: true,
    write: true,
    truncate: false,
};

/// docs/a.txt open for writing, then `docs` trashed remotely and the event
/// applied: both are detached, as clean nodes.
async fn open_then_trashed(
    backend: &FakeBackend,
    dir: &std::path::Path,
) -> (Arc<Engine>, Ino, Ino, u64) {
    let (engine, docs, docs_link) = listed_lazy(backend, dir).await;
    let a_ino = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;
    let (handle, _) = engine.open_file(a_ino, READ_WRITE).expect("open");
    backend.trash(&[docs_link]).await.expect("trash");
    engine.sync_events().await.expect("events");
    assert!(
        engine
            .tree
            .read()
            .get(a_ino)
            .is_some_and(|node| !node.attached)
    );
    assert!(hot(|| engine.lookup(ROOT_INO, b"docs")).is_ok_and(|l| !matches!(l, Lookup::Found(_))));
    (engine, docs, a_ino, handle)
}

#[tokio::test]
async fn a_write_through_a_handle_brings_back_a_file_trashed_remotely() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, a_ino, handle) = open_then_trashed(&backend, dir.path()).await;

    engine
        .write(handle, 0, b"HELLO".to_vec())
        .await
        .expect("write");
    engine.release_file(handle);
    {
        let tree = engine.tree.read();
        assert_eq!(tree.path(a_ino).as_deref(), Some("/docs/a.txt"));
        assert_eq!(tree.path(docs).as_deref(), Some("/docs"));
        assert!(tree.get(docs).is_some_and(|node| node.remote.is_none()));
    }
    assert_eq!(kinds(&engine), [ConflictKind::RemoteDeleteLocalEdit]);
    assert_eq!(read_file(&engine, a_ino).await, b"HELLO world");

    engine.start_background();
    settle(&engine).await;
    let tree = fake_tree(&backend).await;
    assert_eq!(tree["/docs/a.txt"].content, b"HELLO world");
    assert_eq!(
        tree.keys().filter(|path| path.starts_with("/docs")).count(),
        2,
        "only the chain comes back: {:?}",
        tree.keys().collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_truncate_through_a_handle_brings_back_a_file_under_a_conflict_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _docs, a_ino, handle) = open_then_trashed(&backend, dir.path()).await;
    // Another device made a new `docs` meanwhile.
    let root = backend.root().await.expect("root").link_id;
    backend
        .create_folder(&root, "docs", UNIX_EPOCH)
        .await
        .expect("mkdir");
    engine.sync_events().await.expect("events");
    assert!(matches!(
        hot(|| engine.lookup(ROOT_INO, b"docs")).expect("lookup"),
        Lookup::Found(_)
    ));

    engine
        .setattr_slow(
            a_ino,
            SetAttr {
                size: Some(5),
                ..SetAttr::default()
            },
        )
        .await
        .expect("truncate");
    engine.release_file(handle);
    let path = engine.tree.read().path(a_ino).expect("attached");
    assert!(
        path.starts_with("/docs (conflict ") && path.ends_with("/a.txt"),
        "{path}"
    );

    engine.start_background();
    settle(&engine).await;
    let tree = fake_tree(&backend).await;
    assert_eq!(tree[&path].content, b"hello");
    assert!(
        tree.contains_key("/docs"),
        "the other device's folder stays"
    );
}

#[tokio::test]
async fn a_write_after_a_local_unlink_does_not_bring_the_file_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, _docs_link) = listed_lazy(&backend, dir.path()).await;
    let a_ino = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;
    let (handle, _) = engine.open_file(a_ino, READ_WRITE).expect("open");
    engine.unlink(docs, b"a.txt").expect("unlink");
    engine
        .write(handle, 0, b"HELLO".to_vec())
        .await
        .expect("write");
    engine.release_file(handle);
    assert!(engine.tree.read().path(a_ino).is_none());
    assert!(kinds(&engine).is_empty());

    engine.start_background();
    settle(&engine).await;
    assert!(!fake_tree(&backend).await.contains_key("/docs/a.txt"));
}
