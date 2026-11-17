//! A cycle broken through `.pdmount-swap-<ino>` (D117) whose move aside
//! took effect on the drive while its reply was lost (D117 amended, V-7):
//! the retry, an event pass or the next mount sees the node at its own
//! temporary name, which it never takes as its local name.

use super::*;
use std::collections::BTreeMap;

/// What happens between the lost reply and the end.
#[derive(Debug, Clone, Copy)]
enum Then {
    /// The journal retries the step.
    Retry,
    /// An event pass lists the folder before the retry.
    Echo,
    /// The engine stops and the next one goes on.
    Restart,
}

/// Renames `moves` in the root, queued together, over the files `files`;
/// the first rename the drive applies, the move aside, fails with `error`
/// after it took effect. Afterwards every file has its intended name
/// locally and on the drive, no temporary name is left anywhere and no
/// conflict was recorded.
async fn lost_reply_in_a_cycle(
    files: &[(&str, &[u8])],
    moves: &[(&str, &str)],
    error: BackendError,
    then: Then,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = FakeBackend::new();
    let mut engine = running(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    engine.reserve_inos(16).await.expect("inos");
    for (name, data) in files {
        write_file(&engine, ROOT_INO, name, data);
    }
    settle(&engine).await;
    let mut expected: BTreeMap<String, Vec<u8>> = files
        .iter()
        .map(|(name, data)| ((*name).to_owned(), data.to_vec()))
        .collect();
    engine.journal.pause(true);
    for (from, to) in moves {
        engine
            .rename(
                ROOT_INO,
                from.as_bytes(),
                ROOT_INO,
                to.as_bytes(),
                RenameMode::Replace,
            )
            .expect("rename");
        let content = expected.remove(*from).expect("moved file");
        expected.insert((*to).to_owned(), content);
    }
    let before = backend.changes().len();
    backend.lose_rename_reply(error);
    engine.journal.pause(false);
    if !matches!(then, Then::Retry) {
        // The move aside took effect: hold the retry back.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !backend.changes()[before..].iter().any(|change| {
            matches!(change, FakeChange::Renamed { name, .. }
                if name.starts_with(crate::journal::SWAP_PREFIX))
        }) {
            assert!(Instant::now() < deadline, "no move aside");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        engine.journal.pause(true);
        while !engine.journal.idle() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        match then {
            Then::Echo => {
                engine.sync_events().await.expect("events");
                assert_no_swap_name(&engine);
                engine.journal.pause(false);
            }
            Then::Restart => {
                engine.shutdown().await.expect("shutdown");
                drop(engine);
                engine = running(&backend, dir.path()).await;
                engine.ensure_listed(ROOT_INO).await.expect("list");
            }
            Then::Retry => unreachable!(),
        }
    }
    settle(&engine).await;
    assert_no_swap_name(&engine);
    for (name, content) in &expected {
        let ino = found(hot(|| engine.lookup(ROOT_INO, name.as_bytes())).expect("lookup")).ino;
        assert_eq!(&read_file(&engine, ino).await, content, "local {name}");
    }
    let drive: BTreeMap<String, Vec<u8>> = fake_tree(&backend)
        .await
        .into_iter()
        .map(|(path, file)| (path.trim_start_matches('/').to_owned(), file.content))
        .collect();
    assert_eq!(drive, expected, "the drive");
    assert!(engine.conflicts().is_empty(), "{:?}", engine.conflicts());
}

fn assert_no_swap_name(engine: &Engine) {
    let names: Vec<String> = engine
        .tree
        .read()
        .dir_entries(ROOT_INO)
        .expect("entries")
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert!(
        !names
            .iter()
            .any(|name| name.starts_with(crate::journal::SWAP_PREFIX)),
        "a temporary name shows locally: {names:?}"
    );
}

const TWO: &[(&str, &[u8])] = &[("a", b"first"), ("b", b"second")];
const SWAP: &[(&str, &str)] = &[("a", "t"), ("b", "a"), ("t", "b")];
const THREE: &[(&str, &[u8])] = &[("a", b"first"), ("b", b"second"), ("d", b"third")];
const ROTATE: &[(&str, &str)] = &[("a", "x"), ("b", "a"), ("d", "b"), ("x", "d")];

fn lost() -> BackendError {
    BackendError::Transient("reply lost".into())
}

#[tokio::test]
async fn a_lost_reply_to_a_swap_move_is_retried_to_the_names_swapped() {
    lost_reply_in_a_cycle(TWO, SWAP, lost(), Then::Retry).await;
}

/// The SDK's own retry of a rename that went through resends the old name
/// hash, and the server answers `NameConflict` (S-25).
#[tokio::test]
async fn a_name_conflict_after_a_swap_move_that_went_through_is_its_own_move() {
    lost_reply_in_a_cycle(TWO, SWAP, BackendError::NameConflict, Then::Retry).await;
}

#[tokio::test]
async fn a_lost_reply_in_a_rotation_of_three_is_retried_to_the_names_rotated() {
    lost_reply_in_a_cycle(THREE, ROTATE, lost(), Then::Retry).await;
}

#[tokio::test]
async fn an_event_pass_after_a_lost_swap_reply_keeps_the_local_names() {
    lost_reply_in_a_cycle(TWO, SWAP, lost(), Then::Echo).await;
}

#[tokio::test]
async fn the_next_mount_after_a_lost_swap_reply_finishes_the_swap() {
    lost_reply_in_a_cycle(THREE, ROTATE, lost(), Then::Restart).await;
}
