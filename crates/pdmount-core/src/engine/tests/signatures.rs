//! Nodes whose signatures do not verify (D132 amended): the refusals and
//! their errno, accepts bound to the digest, the store across a remount,
//! `signatures check` and the notices.

use super::*;
use crate::config::SignatureMode;
use pdmount_backend::SignaturePart;

const RENAME: RenameMode = RenameMode::Replace;

/// An engine over `seeded()` in `mode`, with the root and `docs` listed.
async fn tree_in(
    backend: &FakeBackend,
    dir: &std::path::Path,
    mode: SignatureMode,
) -> (Arc<Engine>, Ino) {
    let mut config = lazy_config();
    config.signatures = mode;
    let (engine, _inval) = open_with(backend, dir, config).await;
    engine.gate.set_interval(Duration::ZERO);
    engine.ensure_listed(ROOT_INO).await.expect("list root");
    let docs = found(hot(|| engine.lookup(ROOT_INO, b"docs")).expect("lookup")).ino;
    engine.ensure_listed(docs).await.expect("list docs");
    (engine, docs)
}

async fn link_of(backend: &FakeBackend, parent: Option<&LinkId>, name: &str) -> LinkId {
    let root = backend.root().await.expect("root").link_id;
    remote_child(backend, parent.unwrap_or(&root), name)
        .await
        .link_id
}

fn denied<T: std::fmt::Debug>(result: Result<T>) {
    match result {
        Err(error) => {
            assert!(matches!(error, CoreError::AccessDenied), "{error:?}");
            assert_eq!(error.errno(), nix::libc::EACCES);
        }
        Ok(value) => panic!("not refused: {value:?}"),
    }
}

/// Each part alone: the file is listed and stats, but opening, renaming,
/// moving and deleting it answer EACCES; after an accept it opens.
#[tokio::test]
async fn an_unverified_file_is_refused_with_eacces_until_accepted() {
    for part in SignaturePart::ALL {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = seeded().await;
        let top = link_of(&backend, None, "top.bin").await;
        backend
            .set_unverified(&top, part, true, Some("x@example.test"))
            .expect("mark");
        let (engine, docs) = tree_in(&backend, dir.path(), SignatureMode::Refuse).await;
        let ino = found(hot(|| engine.lookup(ROOT_INO, b"top.bin")).expect("lookup")).ino;
        engine.getattr(ino).expect("stat works");
        denied(engine.open_file(ino, READ));
        denied(engine.open_file(ino, WRITE));
        denied(engine.create(ROOT_INO, b"top.bin", 0o644, false, WRITE));
        denied(engine.rename(ROOT_INO, b"top.bin", ROOT_INO, b"other", RENAME));
        denied(engine.rename(ROOT_INO, b"top.bin", docs, b"top.bin", RENAME));
        denied(engine.unlink(ROOT_INO, b"top.bin"));
        denied(engine.setattr(
            ino,
            &SetAttr {
                mode: Some(0o600),
                ..SetAttr::default()
            },
        ));
        // Replacing it is deleting it.
        write_file(&engine, ROOT_INO, "new", b"n");
        denied(engine.rename(ROOT_INO, b"new", ROOT_INO, b"top.bin", RENAME));

        let list = engine.signatures_list().await.expect("list");
        assert_eq!(list.len(), 1, "{list:?}");
        assert_eq!(list[0].path.as_deref(), Some("/top.bin"));
        assert_eq!(list[0].parts, [part.as_str()]);
        assert_eq!(list[0].signer, "x@example.test");
        assert!(list[0].refused && !list[0].accepted);
        assert_eq!(engine.signature_counts(), (1, 1));

        let accepted = engine
            .signature_accept(&list[0].id)
            .await
            .expect("accept")
            .expect("found");
        assert!(accepted.accepted && !accepted.refused);
        assert_eq!(engine.signature_counts(), (1, 0));
        assert_eq!(read_file(&engine, ino).await, b"0123456789");
        engine
            .rename(ROOT_INO, b"top.bin", ROOT_INO, b"renamed.bin", RENAME)
            .expect("rename after the accept");
        assert!(
            engine
                .signature_accept("00000000")
                .await
                .expect("accept")
                .is_none(),
            "an unknown id"
        );
    }
}

/// `warn` records the node and refuses nothing.
#[tokio::test]
async fn warn_only_records_an_unverified_node() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let docs_link = link_of(&backend, None, "docs").await;
    backend
        .set_unverified(&docs_link, SignaturePart::Key, true, None)
        .expect("mark");
    let (engine, docs) = tree_in(&backend, dir.path(), SignatureMode::Warn).await;
    write_file(&engine, docs, "in.txt", b"x");
    engine.opendir(docs).expect("opendir");
    let list = engine.signatures_list().await.expect("list");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].signer, "anonymous");
    assert!(!list[0].refused);
    assert_eq!(engine.signature_counts(), (1, 0));
}

/// A folder whose key did not verify takes no new entry and no move;
/// one whose name alone did not verify does, but is itself refused.
#[tokio::test]
async fn a_folder_whose_key_does_not_verify_takes_nothing_in() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let docs_link = link_of(&backend, None, "docs").await;
    backend
        .set_unverified(&docs_link, SignaturePart::Key, true, None)
        .expect("mark");
    let (engine, docs) = tree_in(&backend, dir.path(), SignatureMode::Refuse).await;
    denied(engine.create(docs, b"new.txt", 0o644, true, WRITE));
    denied(engine.mkdir(docs, b"sub", 0o755));
    denied(engine.rename(ROOT_INO, b"top.bin", docs, b"top.bin", RENAME));
    denied(engine.rename(docs, b"a.txt", docs, b"b.txt", RENAME));
    denied(engine.opendir(docs));
    denied(engine.rmdir(ROOT_INO, b"docs"));
    // Its children are nodes of their own.
    let a = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;
    assert_eq!(read_file(&engine, a).await, b"hello world");

    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let docs_link = link_of(&backend, None, "docs").await;
    backend
        .set_unverified(&docs_link, SignaturePart::Name, true, None)
        .expect("mark");
    let (engine, docs) = tree_in(&backend, dir.path(), SignatureMode::Refuse).await;
    write_file(&engine, docs, "new.txt", b"x");
    engine
        .rename(ROOT_INO, b"top.bin", docs, b"top.bin", RENAME)
        .expect("move in");
    denied(engine.rename(ROOT_INO, b"docs", ROOT_INO, b"docs2", RENAME));
    denied(engine.opendir(docs));
}

/// An accept holds while the failures stay the same or fewer; another
/// signer asks again. A node that verifies again leaves the list.
#[tokio::test]
async fn an_accept_is_bound_to_the_digest() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let top = link_of(&backend, None, "top.bin").await;
    backend
        .set_unverified(&top, SignaturePart::Key, true, Some("x@example.test"))
        .expect("mark");
    backend
        .set_unverified(&top, SignaturePart::Name, true, Some("x@example.test"))
        .expect("mark");
    let (engine, _docs) = tree_in(&backend, dir.path(), SignatureMode::Refuse).await;
    let ino = found(hot(|| engine.lookup(ROOT_INO, b"top.bin")).expect("lookup")).ino;
    let id = engine.signatures_list().await.expect("list")[0].id.clone();
    engine.signature_accept(&id).await.expect("accept");
    engine
        .open_file(ino, READ)
        .map(|(h, _)| engine.release_file(h))
        .expect("open");

    // Fewer failures: still accepted.
    backend
        .set_unverified(&top, SignaturePart::Name, false, None)
        .expect("clear");
    engine.sync_events().await.expect("events");
    engine
        .open_file(ino, READ)
        .map(|(h, _)| engine.release_file(h))
        .expect("open");

    // Another signer: asked again.
    backend
        .set_unverified(&top, SignaturePart::Key, true, Some("y@example.test"))
        .expect("mark");
    engine.sync_events().await.expect("events");
    denied(engine.open_file(ino, READ));
    let list = engine.signatures_list().await.expect("list");
    assert_eq!(list[0].signer, "y@example.test");
    assert!(list[0].refused && !list[0].accepted);
    assert_eq!(list[0].id, id, "the same node, the same id");

    // Verified again: gone from the list, opens.
    backend
        .set_unverified(&top, SignaturePart::Key, false, None)
        .expect("clear");
    engine.sync_events().await.expect("events");
    assert!(engine.signatures_list().await.expect("list").is_empty());
    assert_eq!(engine.signature_counts(), (0, 0));
    engine
        .open_file(ino, READ)
        .map(|(h, _)| engine.release_file(h))
        .expect("open");
}

/// The found nodes and the accepts are in the tree's store: a remount
/// refuses and lists without listing anything first.
#[tokio::test]
async fn found_nodes_and_accepts_survive_a_remount() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let top = link_of(&backend, None, "top.bin").await;
    let docs_link = link_of(&backend, None, "docs").await;
    let a = link_of(&backend, Some(&docs_link), "a.txt").await;
    for link in [&top, &a] {
        backend
            .set_unverified(link, SignaturePart::ContentKey, true, None)
            .expect("mark");
    }
    let accepted = {
        let (engine, _docs) = tree_in(&backend, dir.path(), SignatureMode::Refuse).await;
        let list = engine.signatures_list().await.expect("list");
        assert_eq!(list.len(), 2);
        let top_id = crate::signatures::id_of(&top);
        engine.signature_accept(&top_id).await.expect("accept");
        engine.shutdown().await.expect("shutdown");
        top_id
    };
    let mut config = lazy_config();
    config.signatures = SignatureMode::Refuse;
    let (engine, _inval) = open_with(&backend, dir.path(), config).await;
    let listings = listings(&engine);
    let list = engine.signatures_list().await.expect("list");
    assert_eq!(list.len(), 2, "{list:?}");
    let by_path = |path: &str| {
        list.iter()
            .find(|v| v.path.as_deref() == Some(path))
            .expect(path)
    };
    assert!(by_path("/top.bin").accepted);
    assert_eq!(by_path("/top.bin").id, accepted);
    assert!(by_path("/docs/a.txt").refused);
    let docs = found(hot(|| engine.lookup(ROOT_INO, b"docs")).expect("lookup")).ino;
    let a_ino = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;
    denied(engine.open_file(a_ino, READ));
    let top_ino = found(hot(|| engine.lookup(ROOT_INO, b"top.bin")).expect("lookup")).ino;
    engine
        .open_file(top_ino, READ)
        .map(|(h, _)| engine.release_file(h))
        .expect("open");
    assert_eq!(
        listings,
        crate::engine::tests::listings(&engine),
        "nothing listed"
    );
}

/// `check` lists every folder under the path from the server, folders
/// never listed on this device included, and counts what it decoded.
#[tokio::test]
async fn check_finds_a_node_in_a_folder_never_listed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let docs_link = link_of(&backend, None, "docs").await;
    let deep = backend
        .create_folder(&docs_link, "deep", UNIX_EPOCH)
        .await
        .expect("mkdir");
    let (hidden, _) = backend
        .upload_new(
            &deep,
            "hidden.txt",
            UploadSource::Bytes(Bytes::from_static(b"h")),
            meta(),
        )
        .await
        .expect("upload");
    backend
        .set_unverified(&hidden, SignaturePart::Name, true, None)
        .expect("mark");
    let mut config = lazy_config();
    config.signatures = SignatureMode::Refuse;
    let (engine, _inval) = open_with(&backend, dir.path(), config).await;
    engine.gate.set_interval(Duration::ZERO);
    engine.ensure_listed(ROOT_INO).await.expect("root only");
    let mut notices = engine.subscribe_signatures();
    assert!(engine.signatures_list().await.expect("list").is_empty());

    let progress = CheckProgress::default();
    let counts = engine
        .signatures_check(Some("/docs"), &progress)
        .await
        .expect("check");
    assert_eq!(counts.folders, 2, "docs and deep");
    assert_eq!(counts.nodes, 3, "a.txt, deep, hidden.txt");
    assert_eq!(counts.unverified, 1);
    assert_eq!(counts.failed, 0);
    assert_eq!(progress.counts(), counts);
    let list = engine.signatures_list().await.expect("list");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].path.as_deref(), Some("/docs/deep/hidden.txt"));
    assert_eq!(notices.try_recv().expect("a notice"), 1);

    // Again: found before, nothing new to announce; the whole tree.
    let counts = engine
        .signatures_check(None, &CheckProgress::default())
        .await
        .expect("check");
    assert_eq!((counts.folders, counts.nodes, counts.unverified), (3, 5, 1));
    assert!(notices.try_recv().is_err(), "nothing newly found");
}
