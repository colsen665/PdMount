//! The bulk-read guard in the engine (D081, D111).

use super::*;

/// A drive of `count` files of `size` bytes in the root, in blocks of
/// `block` bytes, listed by an engine whose guard `scan` sets (no spacing of
/// scan downloads unless it says so), and their inode numbers in order.
async fn scan_drive(
    dir: &std::path::Path,
    count: usize,
    size: usize,
    block: usize,
    scan: impl FnOnce(&mut crate::config::ScanConfig),
) -> (FakeBackend, Arc<Engine>, Vec<Ino>) {
    let backend = FakeBackend::new();
    backend.set_block_size(block);
    let root = backend.root().await.expect("root").link_id;
    for n in 0..count {
        let content: Vec<u8> = (0..size).map(|i| (i + n) as u8).collect();
        backend
            .upload_new(
                &root,
                &format!("f{n:03}"),
                UploadSource::Bytes(Bytes::from(content)),
                meta(),
            )
            .await
            .expect("upload");
    }
    let mut config = config();
    config.scan.interval = HumanDuration(Duration::ZERO);
    scan(&mut config.scan);
    let (engine, _inval) = open_with(&backend, dir, config).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let inos = (0..count)
        .map(|n| {
            let name = format!("f{n:03}");
            found(engine.lookup(ROOT_INO, name.as_bytes()).expect("lookup")).ino
        })
        .collect();
    (backend, engine, inos)
}

fn count(engine: &Engine, counter: impl Fn(&Metrics) -> &AtomicU64) -> u64 {
    counter(engine.metrics()).load(Ordering::Relaxed)
}

/// Whether the first block of `ino`'s content is in the disk cache.
fn first_block_on_disk(engine: &Engine, ino: Ino) -> bool {
    let Content::Remote {
        link,
        rev: Some(rev),
    } = engine.tree.read().get(ino).expect("node").content.clone()
    else {
        panic!("remote content");
    };
    engine.cache.on_disk(&BlockKey::new(&link, &rev, 1))
}

/// A file opened alone is read interactively; many different files opened
/// in quick succession become a scan, whose reads get scan priority and no
/// readahead.
#[tokio::test]
async fn a_burst_of_opens_becomes_a_scan_and_a_single_open_stays_interactive() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_backend, engine, inos) = scan_drive(dir.path(), 12, 64, 16, |_| {}).await;
    // One file, read sequentially: interactive, with readahead.
    let (handle, _) = engine.open_file(inos[0], READ).expect("open");
    assert!(!engine.file(handle).expect("handle").scan);
    engine.read(handle, 0, 16).await.expect("read");
    engine.read(handle, 16, 16).await.expect("read");
    engine.release_file(handle);
    assert_eq!(count(&engine, |m| &m.scan_reads), 0);
    eventually("readahead", || {
        count(&engine, |m| &m.prefetched_blocks) == 2
    })
    .await;
    // Eleven more at once: the tenth different file begins the scan.
    let handles: Vec<u64> = inos[1..]
        .iter()
        .map(|&ino| engine.open_file(ino, READ).expect("open").0)
        .collect();
    assert!(engine.scan_guard().scanning());
    assert_eq!(count(&engine, |m| &m.scans), 1);
    let scan: Vec<bool> = handles
        .iter()
        .map(|&handle| engine.file(handle).expect("handle").scan)
        .collect();
    assert_eq!(scan, [[false; 8].as_slice(), &[true; 3]].concat());
    for &handle in &handles {
        // Sequential reads, which start readahead when interactive.
        engine.read(handle, 0, 16).await.expect("read");
        engine.read(handle, 16, 16).await.expect("read");
    }
    assert_eq!(count(&engine, |m| &m.scan_reads), 6, "two reads of 3 files");
    eventually("readahead", || {
        count(&engine, |m| &m.prefetched_blocks) == 2 + 8 * 2
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        count(&engine, |m| &m.prefetched_blocks),
        2 + 8 * 2,
        "readahead for the 8 interactive handles only"
    );
    for handle in handles {
        engine.release_file(handle);
    }
    // Opened for writing: never part of a scan.
    let (writer, _) = engine.open_file(inos[0], WRITE).expect("open");
    assert!(!engine.file(writer).expect("handle").scan);
    engine.release_file(writer);
}

/// A handle opened during a scan reads its first bytes as a scan, and
/// interactively once it read past them.
#[tokio::test]
async fn a_read_past_the_first_bytes_is_interactive() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_backend, engine, inos) = scan_drive(dir.path(), 2, 64, 16, |scan| {
        scan.opens = 2;
        scan.first_bytes = crate::units::ByteSize(32);
    })
    .await;
    engine.open_file(inos[0], READ).expect("open");
    let (handle, _) = engine.open_file(inos[1], READ).expect("open");
    assert!(engine.file(handle).expect("handle").scan);
    for offset in [0, 16] {
        engine.read(handle, offset, 16).await.expect("read");
    }
    assert_eq!(count(&engine, |m| &m.scan_reads), 2, "the first 32 bytes");
    assert_eq!(count(&engine, |m| &m.prefetched_blocks), 0);
    engine.read(handle, 32, 16).await.expect("read");
    assert_eq!(
        count(&engine, |m| &m.scan_reads),
        2,
        "past them: interactive"
    );
    // The run goes on, and readahead with it.
    eventually("readahead", || count(&engine, |m| &m.prefetched_blocks) > 0).await;
}

/// A scan read waits while a request a user waits on is in flight, so a
/// scan cannot delay it; it goes once that ends.
#[tokio::test]
async fn a_scan_read_waits_for_interactive_requests() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_backend, engine, inos) = scan_drive(dir.path(), 2, 64, 16, |scan| scan.opens = 2).await;
    engine.open_file(inos[0], READ).expect("open");
    let (handle, _) = engine.open_file(inos[1], READ).expect("open");
    let waiting = engine.user_wait();
    let read = tokio::spawn({
        let engine = Arc::clone(&engine);
        async move { engine.read(handle, 0, 16).await.map(ReadData::into_vec) }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !read.is_finished(),
        "a scan read went ahead of a user's request"
    );
    drop(waiting);
    let data = read.await.expect("join").expect("read");
    assert_eq!(data.len(), 16);
}

/// Scan downloads start `scan.interval` apart, over all the session's
/// trees; reads served from the cache take no turn.
#[tokio::test]
async fn scan_downloads_are_paced_over_the_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_backend, engine, inos) = scan_drive(dir.path(), 6, 64, 64, |scan| {
        scan.opens = 2;
        scan.interval = HumanDuration(Duration::from_millis(200));
    })
    .await;
    let handles: Vec<u64> = inos
        .iter()
        .map(|&ino| engine.open_file(ino, READ).expect("open").0)
        .collect();
    let started = std::time::Instant::now();
    // Five scan handles (the first opened alone), read at once.
    let reads: Vec<_> = handles[1..]
        .iter()
        .map(|&handle| {
            let engine = Arc::clone(&engine);
            tokio::spawn(async move { engine.read(handle, 0, 64).await.map(drop) })
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
    // Cached now: no turn, no wait.
    let again = std::time::Instant::now();
    for &handle in &handles[1..] {
        engine.read(handle, 0, 64).await.expect("read");
    }
    assert!(again.elapsed() < Duration::from_millis(200));
}

/// Blocks scan reads bring in stay within `scan.cache_share` of the cap,
/// the oldest evicted first, so the working set stays on disk; a scan
/// block an interactive read uses leaves the scan blocks.
#[tokio::test]
async fn scan_blocks_stay_within_their_share_and_spare_the_working_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    // The tests' cap is 1 MiB: 10 % hold six 16 KiB blocks.
    let block = 16 << 10;
    let (_backend, engine, inos) = scan_drive(dir.path(), 31, block, block, |_| {}).await;
    // The working set, read interactively first.
    assert_eq!(read_file(&engine, inos[0]).await.len(), block);
    // Opened before the scan, read after it.
    let (early, _) = engine.open_file(inos[30], READ).expect("open");
    for &ino in &inos[1..] {
        let (handle, _) = engine.open_file(ino, READ).expect("open");
        engine.read(handle, 0, block as u32).await.expect("read");
        engine.release_file(handle);
    }
    let guard = engine.scan_guard();
    assert!(count(&engine, |m| &m.scan_reads) >= 20);
    assert!(guard.held() > 0, "scan blocks are tracked");
    assert!(
        guard.held() <= guard.limit(),
        "{} bytes of scan blocks, over the limit of {}",
        guard.held(),
        guard.limit()
    );
    assert!(count(&engine, |m| &m.scan_evictions) > 0);
    assert!(
        first_block_on_disk(&engine, inos[0]),
        "the working set stays"
    );
    assert!(
        first_block_on_disk(&engine, inos[5]),
        "read before the scan began"
    );
    assert!(
        !first_block_on_disk(&engine, inos[12]),
        "an old scan block went first"
    );
    assert!(
        first_block_on_disk(&engine, inos[30]),
        "the newest scan block stays"
    );
    // An interactive read of the newest scan block takes it out of them.
    let held = guard.held();
    let disk = (block + crate::pool::SEAL_OVERHEAD) as u64;
    engine.read(early, 0, block as u32).await.expect("read");
    assert_eq!(guard.held(), held - disk);
    engine.release_file(early);
}
