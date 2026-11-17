//! Reading other devices' pending markers: the cap, one notice per pass,
//! no reading behind a closed write gate, a time out of range, and the
//! checks before this device's own marker is trashed (D126, V-4, V-8).

use super::*;

/// An engine of device `dev-b` on `backend` that has listed its root.
async fn reader(backend: &FakeBackend, dir: &std::path::Path) -> Arc<Engine> {
    let (engine, _inval) = open_device(backend, dir, config(), "dev-b").await;
    engine.gate.set_interval(Duration::ZERO);
    engine.ensure_listed(ROOT_INO).await.expect("list");
    engine
}

fn body(device: &str) -> Vec<u8> {
    marker::encode(Some(device), CLOCK_SECS, 1, 10)
}

/// 40 markers: 16 are read and shown, 24 counted; one notice for the pass
/// names the 16 and the count. A marker planted later does not push out
/// the devices already shown.
#[tokio::test]
async fn markers_beyond_the_cap_are_counted_and_announced_in_one_notice() {
    let backend = marker_drive().await;
    for n in 0..40 {
        let name = format!("m{n:02}");
        upload_marker(&backend, &name, &body(&name)).await;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let engine = reader(&backend, dir.path()).await;
    let mut notices = engine.subscribe_incoming();
    engine.scan_markers().await.expect("scan");
    let incoming = engine.incoming_markers();
    assert_eq!(incoming.len(), marker::MAX_MARKERS_READ, "{incoming:?}");
    assert_eq!(engine.incoming_not_read(), 24);
    let notice = notices.try_recv().expect("one notice");
    assert_eq!(notice.markers.len(), marker::MAX_MARKERS_READ);
    assert_eq!(notice.not_read, 24);
    assert!(notices.try_recv().is_err(), "one notice per pass");
    let shown: Vec<Option<String>> = incoming.iter().map(|m| m.device.clone()).collect();

    // A device whose id sorts first, planted after: not read in place of
    // one already shown.
    upload_marker(&backend, "a", &body("a")).await;
    engine.sync_events().await.expect("events");
    engine.scan_markers().await.expect("scan");
    let again: Vec<Option<String>> = engine
        .incoming_markers()
        .iter()
        .map(|m| m.device.clone())
        .collect();
    assert_eq!(again, shown);
    assert_eq!(engine.incoming_not_read(), 25);
    assert!(notices.try_recv().is_err(), "nothing new was read");
}

/// A tree whose write gate is closed (a viewer's share) reads no marker
/// and announces none; open, it reads them, and what was announced is not
/// announced again when the gate closes and opens.
#[tokio::test]
async fn a_tree_whose_write_gate_is_closed_reads_no_markers() {
    let backend = marker_drive().await;
    for name in ["m1", "m2"] {
        upload_marker(&backend, name, &body(name)).await;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let engine = reader(&backend, dir.path()).await;
    let mut notices = engine.subscribe_incoming();
    engine.set_write_gate(false);
    // A fault the first marker fetch takes: still there after the closed
    // gate's pass if that pass fetched nothing.
    backend.inject_failure(
        pdmount_backend::fake::FakeOp::Blocks,
        BackendError::Transient("once".into()),
    );
    engine.scan_markers().await.expect("scan");
    assert!(engine.incoming_markers().is_empty());
    assert!(notices.try_recv().is_err(), "nothing announced");

    engine.set_write_gate(true);
    engine.scan_markers().await.expect("scan");
    assert_eq!(
        engine.incoming_markers().len(),
        1,
        "the first fetch took the fault, so the closed gate fetched nothing"
    );
    assert_eq!(notices.try_recv().expect("a notice").markers.len(), 1);
    engine.scan_markers().await.expect("scan");
    assert_eq!(engine.incoming_markers().len(), 2);
    assert_eq!(notices.try_recv().expect("a notice").markers.len(), 1);

    engine.set_write_gate(false);
    engine.scan_markers().await.expect("scan");
    assert!(engine.incoming_markers().is_empty(), "not shown any more");
    engine.set_write_gate(true);
    engine.scan_markers().await.expect("scan");
    assert_eq!(engine.incoming_markers().len(), 2);
    assert!(notices.try_recv().is_err(), "announced twice");
}

/// A marker whose `time` no clock can hold is ignored, the others are read,
/// and nothing panics, also after a restart (V-8).
#[tokio::test]
async fn a_marker_time_out_of_range_is_ignored_and_the_others_are_read() {
    let backend = marker_drive().await;
    upload_marker(
        &backend,
        "bad",
        format!(
            r#"{{"version":1,"time":{},"entries":1,"bytes":1}}"#,
            u64::MAX
        )
        .as_bytes(),
    )
    .await;
    upload_marker(&backend, "good", &body("good")).await;
    let dir = tempfile::tempdir().expect("tempdir");
    for _run in 0..2 {
        let engine = reader(&backend, dir.path()).await;
        let mut notices = engine.subscribe_incoming();
        engine.scan_markers().await.expect("scan");
        let incoming = engine.incoming_markers();
        assert_eq!(incoming.len(), 1, "{incoming:?}");
        assert_eq!(incoming[0].device.as_deref(), Some("good"));
        let _ = notices.try_recv();
        engine.shutdown().await.expect("shutdown");
    }
}

/// A link adopted as this device's marker (a file of its name in
/// `.pdmount`) that was renamed, or moved out of the folder, before the
/// drain is forgotten, not trashed (V-4).
#[tokio::test]
async fn an_adopted_marker_renamed_or_moved_is_not_trashed() {
    for moved in [false, true] {
        let backend = marker_drive().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let (engine, _inval) = open_with(&backend, dir.path(), marker_config()).await;
        engine.gate.set_interval(Duration::ZERO);
        engine.ensure_listed(ROOT_INO).await.expect("list");
        // Something queued, so the drain waits until the link has moved.
        write_file(&engine, ROOT_INO, "f.txt", b"f");
        upload_marker(&backend, "dev-a", &body("planted")).await;
        engine.sync_events().await.expect("events");
        engine.scan_markers().await.expect("scan");
        let link = engine.own_pending_marker().expect("adopted");
        let name = marker::marker_name(&test_device("dev-a").id);
        let root = backend.root().await.expect("root").link_id;
        let expect = if moved {
            let docs = backend
                .list_children(&root)
                .await
                .expect("list")
                .into_iter()
                .find(|node| node.name == "docs")
                .expect("docs")
                .link_id;
            backend
                .rename(&link, Some(&docs), &name, None)
                .await
                .expect("move");
            format!("/docs/{name}")
        } else {
            backend
                .rename(&link, None, "kept.txt", None)
                .await
                .expect("rename");
            format!("/{}/kept.txt", marker::MARKER_DIR)
        };
        engine.start_background();
        settle(&engine).await;
        wait_for("the watcher", || async {
            engine.own_pending_marker().is_none()
        })
        .await;
        assert!(
            fake_tree(&backend).await.contains_key(&expect),
            "{expect} was trashed"
        );
        engine.shutdown().await.expect("shutdown");
    }
}
