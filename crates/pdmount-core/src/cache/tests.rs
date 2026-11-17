use super::block_cache::DOWNLOADS;
use super::disk::DiskEntry;
use super::*;
use pdmount_backend::fake::FakeBackend;
use pdmount_backend::{FileMeta, UploadSource};
use std::time::Duration;

fn infos(sizes: &[Option<u32>]) -> Vec<BlockInfo> {
    sizes
        .iter()
        .enumerate()
        .map(|(i, &plain_size)| BlockInfo {
            index: i as u32 + 1,
            plain_size,
        })
        .collect()
}

#[test]
fn layout_uses_declared_sizes() {
    let layout = Layout::new(&infos(&[Some(10), Some(5)]), 15).expect("layout");
    assert_eq!(
        layout.blocks[1],
        BlockSpan {
            index: 2,
            offset: 10,
            len: 5
        }
    );
    assert_eq!(layout.size(), 15);
}

#[test]
fn declared_sizes_must_fit_blocks_and_the_file() {
    let integrity = |result: Result<Layout>| {
        matches!(
            result,
            Err(CoreError::Backend(
                pdmount_backend::BackendError::Integrity(_)
            ))
        )
    };
    let two_gib = 2u32 << 30;
    assert!(integrity(Layout::new(
        &infos(&[Some(two_gib)]),
        u64::from(two_gib)
    )));
    assert!(integrity(Layout::new(&infos(&[Some(0), Some(5)]), 5)));
    assert!(integrity(Layout::new(&infos(&[Some(10), Some(5)]), 16)));
    assert!(integrity(Layout::new(
        &infos(&[Some(10), Some(5)]),
        1 << 62
    )));
    // An empty last block, as an upload that ends on a block boundary may list.
    assert!(Layout::new(&infos(&[Some(4), Some(0)]), 4).is_ok());
}

#[test]
fn layout_without_sizes_assumes_4_mib_blocks() {
    let size = BLOCK_SIZE * 2 + 7;
    let layout = Layout::new(&infos(&[None, None, None]), size).expect("layout");
    assert_eq!(layout.blocks[2].offset, BLOCK_SIZE * 2);
    assert_eq!(layout.blocks[2].len, 7);
    assert!(Layout::new(&infos(&[None, None]), BLOCK_SIZE * 2 + 1).is_err());
    assert!(Layout::new(&infos(&[None, None]), BLOCK_SIZE).is_err());
    assert!(Layout::new(&infos(&[]), 0).is_ok());
    assert!(
        Layout::new(&infos(&[]), 5).is_err(),
        "no blocks cannot hold a non-empty file"
    );
}

#[test]
fn spans_and_assembly_cover_partial_ranges() {
    let layout = Layout::new(&infos(&[Some(4), Some(4), Some(4)]), 12).expect("layout");
    let spans = layout.spans(3, 6);
    assert_eq!(
        spans.iter().map(|s| s.index).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    let blocks = [&b"abcd"[..], b"efgh", b"ijkl"];
    let mut read = ReadBuf::new(Buffer::heap(6), 3, 6);
    for (span, data) in spans.iter().zip(blocks) {
        read.append(span, data);
    }
    assert_eq!(read.finish(), b"defghi");
    let one = ReadData::of_block(&spans[1], Bytes::from_static(b"efgh"), 5, 2);
    assert_eq!(one, b"fg");
    assert!(layout.spans(12, 5).is_empty());
    assert_eq!(layout.spans(4, 4).len(), 1);
}

fn disk_get(cache: &BlockCache, key: &BlockKey) -> Option<Bytes> {
    let (buffer, len) = cache
        .disk
        .read(key, Buffer::heap, &cache.store, &cache.metrics)?;
    Some(buffer.freeze(len))
}

async fn setup(dir: &Path, disk_bytes: u64) -> (Arc<BlockCache>, Arc<Store>, LinkId, RevisionId) {
    let (cache, store, link, rev, _) = setup_with(dir, disk_bytes, b"0123456789").await;
    (cache, store, link, rev)
}

/// A cache over a fake drive holding one file `f` with 4-byte blocks.
async fn setup_with(
    dir: &Path,
    disk_bytes: u64,
    content: &'static [u8],
) -> (Arc<BlockCache>, Arc<Store>, LinkId, RevisionId, FakeBackend) {
    let pool = Pool::new(16 << 20).expect("pool");
    setup_pooled(dir, disk_bytes, content, pool).await
}

async fn setup_pooled(
    dir: &Path,
    disk_bytes: u64,
    content: &'static [u8],
    pool: Pool,
) -> (Arc<BlockCache>, Arc<Store>, LinkId, RevisionId, FakeBackend) {
    setup_shared(dir, disk_bytes, content, pool, None).await
}

async fn setup_shared(
    dir: &Path,
    disk_bytes: u64,
    content: &'static [u8],
    pool: Pool,
    shared: Option<SharedCap>,
) -> (Arc<BlockCache>, Arc<Store>, LinkId, RevisionId, FakeBackend) {
    let backend = FakeBackend::new();
    backend.set_block_size(4);
    let root = backend.root().await.expect("root").link_id;
    let meta = FileMeta {
        mtime: UNIX_EPOCH,
        mode: None,
        media_type: "application/octet-stream".into(),
        sha1: None,
    };
    let (link, rev) = backend
        .upload_new(
            &root,
            "f",
            UploadSource::Bytes(Bytes::from_static(content)),
            meta,
        )
        .await
        .expect("upload");
    let store = Arc::new(Store::open(&dir.join("state.db")).expect("store"));
    let cache = BlockCache::with_pool(
        Arc::new(backend.clone()),
        Arc::clone(&store),
        Arc::default(),
        CacheOptions {
            dir: dir.join("blocks"),
            master_key: Some([7; 32]),
            ram_bytes: 16 << 20,
            disk_bytes,
            shared,
            downloads: None,
        },
        Protections::default(),
        pool,
    )
    .expect("cache");
    (Arc::new(cache), store, link, rev, backend)
}

#[tokio::test]
async fn at_most_five_blocks_download_at_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (cache, _store, link, rev, backend) = setup_with(
        dir.path(),
        1 << 20,
        b"0123456789abcdefghijklmnopqrstuvwxyzABCD",
    )
    .await;
    let layout = cache.layout(&link, &rev, 40).await.expect("layout");
    assert_eq!(layout.blocks.len(), 10);
    backend.set_latency(Duration::from_secs(1));
    let calls = || {
        cache
            .metrics
            .backend_calls
            .load(std::sync::atomic::Ordering::Relaxed)
    };
    let before = calls();
    let priorities = [
        Priority::Interactive,
        Priority::Readahead,
        Priority::Prefetch,
    ];
    let gets: Vec<_> = (1..=10)
        .map(|index: u32| {
            let cache = Arc::clone(&cache);
            let key = BlockKey::new(&link, &rev, index);
            tokio::spawn(async move { cache.get(&key, 4, priorities[index as usize % 3]).await })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(calls() - before, DOWNLOADS as u64);
    for get in gets {
        get.await.expect("join").expect("get");
    }
    assert_eq!(calls() - before, 10);
}

/// A cache over `backend` in `dir`, with `downloads` as its permits.
fn cache_on(
    backend: &FakeBackend,
    dir: &Path,
    downloads: Option<Arc<DownloadSlots>>,
) -> Arc<BlockCache> {
    std::fs::create_dir_all(dir).expect("dir");
    let store = Arc::new(Store::open(&dir.join("state.db")).expect("store"));
    let cache = BlockCache::with_pool(
        Arc::new(backend.clone()),
        store,
        Arc::default(),
        CacheOptions {
            dir: dir.join("blocks"),
            master_key: Some([7; 32]),
            ram_bytes: 16 << 20,
            disk_bytes: 1 << 20,
            shared: None,
            downloads,
        },
        Protections::default(),
        Pool::new(16 << 20).expect("pool"),
    )
    .expect("cache");
    Arc::new(cache)
}

/// The most blocks two caches over one fake have in flight together while
/// each downloads the ten blocks of the same file, at all priorities.
async fn peak_of_two(dir: &Path, downloads: [Option<Arc<DownloadSlots>>; 2]) -> usize {
    let backend = FakeBackend::new();
    backend.set_block_size(4);
    let root = backend.root().await.expect("root").link_id;
    let meta = FileMeta {
        mtime: UNIX_EPOCH,
        mode: None,
        media_type: "application/octet-stream".into(),
        sha1: None,
    };
    let (link, rev) = backend
        .upload_new(
            &root,
            "f",
            UploadSource::Bytes(Bytes::from_static(
                b"0123456789abcdefghijklmnopqrstuvwxyzABCD",
            )),
            meta,
        )
        .await
        .expect("upload");
    let [a, b] = downloads;
    let caches = [
        cache_on(&backend, &dir.join("a"), a),
        cache_on(&backend, &dir.join("b"), b),
    ];
    for cache in &caches {
        assert_eq!(
            cache
                .layout(&link, &rev, 40)
                .await
                .expect("layout")
                .blocks
                .len(),
            10
        );
    }
    backend.set_latency(Duration::from_millis(200));
    backend.take_peak_block_fetches();
    let priorities = [
        Priority::Interactive,
        Priority::Readahead,
        Priority::Prefetch,
    ];
    let gets: Vec<_> = caches
        .iter()
        .flat_map(|cache| {
            (1..=10).map(|index: u32| {
                let cache = Arc::clone(cache);
                let key = BlockKey::new(&link, &rev, index);
                tokio::spawn(
                    async move { cache.get(&key, 4, priorities[index as usize % 3]).await },
                )
            })
        })
        .collect();
    for get in gets {
        get.await.expect("join").expect("get");
    }
    backend.take_peak_block_fetches()
}

/// D033, D110: the caches of a session's trees share its download permits,
/// so together they have at most five blocks in flight; caches with
/// permits of their own have five each.
#[tokio::test]
async fn caches_sharing_the_session_limits_download_five_blocks_at_most() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = Arc::new(DownloadSlots::new());
    let shared = peak_of_two(
        &dir.path().join("shared"),
        [Some(Arc::clone(&session)), Some(Arc::clone(&session))],
    )
    .await;
    assert_eq!(shared, DOWNLOADS, "two trees, one session");
    let own = peak_of_two(&dir.path().join("own"), [None, None]).await;
    assert_eq!(own, 2 * DOWNLOADS, "two caches alone");
}

#[tokio::test]
async fn blocks_are_fetched_once_then_served_locally() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (cache, _store, link, rev) = setup(dir.path(), 1 << 20).await;
    let layout = cache.layout(&link, &rev, 10).await.expect("layout");
    assert_eq!(layout.blocks.len(), 3);
    let key = BlockKey::new(&link, &rev, 2);
    assert_eq!(
        cache
            .get(&key, 4, Priority::Interactive)
            .await
            .expect("get"),
        &b"4567"[..]
    );
    assert_eq!(cache.get_local(&key).expect("local"), &b"4567"[..]);
    assert_eq!(
        cache
            .metrics
            .cache_misses
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

#[tokio::test]
async fn the_disk_cache_survives_reopen_and_rejects_tampering() {
    let dir = tempfile::tempdir().expect("tempdir");
    let key;
    {
        let (cache, store, link, rev) = setup(dir.path(), 1 << 20).await;
        key = BlockKey {
            link,
            rev,
            index: 1,
        };
        cache
            .get(&key, 4, Priority::Interactive)
            .await
            .expect("get");
        store.flush().await.expect("flush");
    }
    let (cache, _store, _, _) = setup(dir.path(), 1 << 20).await;
    // Different fake upload ids are irrelevant: the key is reused as is.
    assert_eq!(disk_get(&cache, &key).expect("disk"), &b"0123"[..]);
    let path = cache.disk.path(&key);
    let mut sealed = std::fs::read(&path).expect("read");
    let last = sealed.len() - 1;
    sealed[last] ^= 1;
    std::fs::write(&path, sealed).expect("write");
    assert!(disk_get(&cache, &key).is_none());
    assert!(!path.exists());
}

/// A start before the keyring answers (D106): the index is there, but
/// no block is read, written or dropped as corrupt until the key comes.
#[tokio::test]
async fn a_cache_without_its_key_keeps_the_disk_blocks_until_unlocked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (link, rev, backend);
    {
        let (cache, store, l, r, b) = setup_with(dir.path(), 1 << 20, b"0123456789").await;
        for index in 1..=2 {
            cache
                .get(&BlockKey::new(&l, &r, index), 4, Priority::Interactive)
                .await
                .expect("get");
        }
        store.flush().await.expect("flush");
        (link, rev, backend) = (l, r, b);
    }
    let store = Arc::new(Store::open(&dir.path().join("state.db")).expect("store"));
    let cache = Arc::new(
        BlockCache::with_pool(
            Arc::new(backend),
            Arc::clone(&store),
            Arc::default(),
            CacheOptions {
                dir: dir.path().join("blocks"),
                master_key: None,
                ram_bytes: 16 << 20,
                disk_bytes: 1 << 20,
                shared: None,
                downloads: None,
            },
            Protections::default(),
            Pool::new(16 << 20).expect("pool"),
        )
        .expect("cache"),
    );
    let first = BlockKey::new(&link, &rev, 1);
    let path = cache.disk.path(&first);
    assert!(!cache.unlocked());
    assert!(cache.get_local(&first).is_none(), "no key, no disk read");
    assert!(!cache.contains(&first));
    assert!(path.exists(), "a locked read must not drop the block");
    assert!(cache.on_disk(&first), "the index stays");
    // A download goes to RAM only: nothing can be sealed.
    let third = BlockKey::new(&link, &rev, 3);
    assert_eq!(
        cache
            .get(&third, 2, Priority::Interactive)
            .await
            .expect("get"),
        &b"89"[..]
    );
    assert!(!cache.on_disk(&third));
    assert_eq!(cache.usage().1, 2);

    assert!(cache.unlock(&[7; 32]));
    assert!(!cache.unlock(&[8; 32]), "the first key stays");
    assert_eq!(disk_get(&cache, &first).expect("disk"), &b"0123"[..]);
    let second = BlockKey::new(&link, &rev, 2);
    cache.ram.lock().entries.clear();
    assert_eq!(cache.get_local(&second).expect("local"), &b"4567"[..]);
}

#[tokio::test]
async fn the_disk_cache_evicts_down_to_80_percent() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Each sealed block is 24 + 4 + 16 = 44 bytes; the limit fits two.
    let (cache, _store, link, rev) = setup(dir.path(), 100).await;
    for index in 1..=3 {
        let key = BlockKey::new(&link, &rev, index);
        let len = if index == 3 { 2 } else { 4 };
        cache
            .get(&key, len, Priority::Interactive)
            .await
            .expect("get");
    }
    let (bytes, _) = cache.usage();
    assert!(bytes <= 80, "{bytes} bytes left");
    assert!(
        cache
            .metrics
            .cache_evictions
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0
    );
}

/// Fetches every block of the 10-byte file `f` (blocks 1, 2 and 3).
async fn fetch_all(cache: &Arc<BlockCache>, link: &LinkId, rev: &RevisionId) -> Vec<BlockKey> {
    let layout = cache.layout(link, rev, 10).await.expect("layout");
    let mut keys = Vec::new();
    for span in &layout.blocks {
        let key = BlockKey::new(link, rev, span.index);
        cache
            .get(&key, span.len, Priority::Interactive)
            .await
            .expect("get");
        keys.push(key);
    }
    keys
}

/// Dates an entry back, as if it was last used at `at`.
fn age(cache: &BlockCache, key: &BlockKey, at: i64) {
    let mut index = cache.disk.index.lock();
    let entry = *index.entries.get(key).expect("entry");
    index.insert(
        key.clone(),
        DiskEntry {
            last_access: at,
            ..entry
        },
    );
}

fn on_disk(cache: &BlockCache, key: &BlockKey) -> bool {
    cache.disk.index.lock().entries.contains_key(key) && cache.disk.path(key).exists()
}

#[tokio::test]
async fn eviction_follows_last_access_including_ram_hits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (cache, _store, link, rev) = setup(dir.path(), 1 << 20).await;
    let keys = fetch_all(&cache, &link, &rev).await;
    age(&cache, &keys[0], 100);
    age(&cache, &keys[1], 200);
    age(&cache, &keys[2], 300);
    // The oldest block is in RAM: this hit must count on the disk too.
    assert!(in_ram(&cache, &keys[0]));
    cache.get_local(&keys[0]).expect("hit");
    // Room for the first block only.
    let room = cache.disk.index.lock().entries[&keys[0]].bytes;
    let evicted = cache.gc(room);
    assert_eq!(evicted.blocks, 2);
    assert!(on_disk(&cache, &keys[0]), "the block just read stays");
    assert!(!on_disk(&cache, &keys[1]));
    assert!(!on_disk(&cache, &keys[2]));
}

#[tokio::test]
async fn last_access_updates_are_batched_into_the_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (cache, store, link, rev) = setup(dir.path(), 1 << 20).await;
    let keys = fetch_all(&cache, &link, &rev).await;
    age(&cache, &keys[0], 100);
    cache.get_local(&keys[0]).expect("hit");
    cache.get_local(&keys[0]).expect("hit");
    assert_eq!(cache.disk.index.lock().touched.len(), 1);
    cache.flush_access();
    assert!(cache.disk.index.lock().touched.is_empty());
    store.flush().await.expect("flush");
    let stored: i64 = store
        .read(|conn| {
            conn.query_row(
                "SELECT last_access FROM cache_blocks WHERE idx = 1",
                [],
                |row| row.get(0),
            )
        })
        .expect("read");
    assert!(stored > 100);
}

#[tokio::test]
async fn protected_revisions_survive_pressure_gc_and_wipe() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(Store::open(&dir.path().join("state.db")).expect("store"));
    let staging = crate::staging::StagingDir::open(&dir.path().join("staging")).expect("dir");
    let backend = FakeBackend::new();
    backend.set_block_size(4);
    let root = backend.root().await.expect("root").link_id;
    let meta = FileMeta {
        mtime: UNIX_EPOCH,
        mode: None,
        media_type: "application/octet-stream".into(),
        sha1: None,
    };
    let (link, rev) = backend
        .upload_new(
            &root,
            "f",
            UploadSource::Bytes(Bytes::from_static(b"0123456789")),
            meta,
        )
        .await
        .expect("upload");
    let cache = Arc::new(
        BlockCache::with_pool(
            Arc::new(backend),
            Arc::clone(&store),
            Arc::default(),
            CacheOptions {
                dir: dir.path().join("blocks"),
                master_key: Some([7; 32]),
                ram_bytes: 16 << 20,
                disk_bytes: 100,
                shared: None,
                downloads: None,
            },
            staging.protections().clone(),
            Pool::new(16 << 20).expect("pool"),
        )
        .expect("cache"),
    );
    let base = crate::staging::Base {
        link: link.clone(),
        rev: rev.clone(),
        rev_size: 10,
        size: 10,
    };
    let mut staged = staging
        .create(2, Some(rev.clone()), Some(base.clone()), 10)
        .expect("stage");
    assert_eq!(cache.disk_stats().protected_revisions, 1);
    // Three blocks are 124 bytes, over the limit of 100.
    let keys = fetch_all(&cache, &link, &rev).await;
    assert!(keys.iter().all(|key| on_disk(&cache, key)));
    assert_eq!(cache.gc(0), Evicted::default());
    assert_eq!(cache.wipe(None), Evicted::default());
    let links = HashSet::from([link.clone()]);
    assert_eq!(cache.wipe(Some(&links)), Evicted::default());
    // Rebasing changes the upload's precondition, not where the bytes
    // come from: the blocks stay protected.
    staged
        .set_base_rev(Some(RevisionId("newer".into())))
        .expect("rebase");
    assert_eq!(cache.gc(0), Evicted::default());
    // Fully materialized, nothing reads the base any more.
    staged.truncate(0).expect("truncate");
    assert_eq!(cache.disk_stats().protected_revisions, 0);
    assert_eq!(cache.gc(0).blocks, 3);
    // Dropping a staging with a base also lets go.
    let staged = staging
        .create(3, Some(rev.clone()), Some(base), 10)
        .expect("stage");
    assert_eq!(cache.disk_stats().protected_revisions, 1);
    drop(staged);
    assert_eq!(cache.disk_stats().protected_revisions, 0);
    // A staging found at mount protects its base again.
    let staged = staging.reopen(3).expect("reopen");
    assert_eq!(cache.disk_stats().protected_revisions, 1);
    drop(staged);
}

#[tokio::test]
async fn pinned_rows_are_kept_and_counted_apart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let keys;
    {
        let (cache, store, link, rev) = setup(dir.path(), 1 << 20).await;
        keys = fetch_all(&cache, &link, &rev).await;
        store.flush().await.expect("flush");
        let index = keys[0].index;
        store.write(move |conn| {
            conn.execute("UPDATE cache_blocks SET pin = 1 WHERE idx = ?1", [index])
                .map(drop)
        });
        store.flush().await.expect("flush");
    }
    let (cache, _store, _, _) = setup(dir.path(), 1 << 20).await;
    let stats = cache.disk_stats();
    assert_eq!(stats.pinned_bytes, 44);
    assert_eq!(stats.bytes, 44 + 42);
    assert_eq!(stats.blocks, 3);
    assert_eq!(cache.gc(0).blocks, 2);
    assert!(on_disk(&cache, &keys[0]));
    assert_eq!(cache.wipe(None), Evicted::default());
    assert!(on_disk(&cache, &keys[0]));
}

#[tokio::test]
async fn pinned_revisions_hold_present_and_future_blocks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (cache, store, link, rev) = setup(dir.path(), 100).await;
    let key = |index| BlockKey::new(&link, &rev, index);
    cache
        .get(&key(1), 4, Priority::Interactive)
        .await
        .expect("get");
    let revision = HashSet::from([(link.clone(), rev.clone())]);
    cache.set_pinned_revisions(&revision);
    assert_eq!(cache.disk_stats().pinned_bytes, 44);
    assert_eq!(cache.disk_stats().bytes, 0);
    // Blocks arriving later are pinned too, and past the cap of 100.
    for (index, len) in [(2, 4), (3, 2)] {
        cache
            .get(&key(index), len, Priority::Prefetch)
            .await
            .expect("get");
    }
    assert_eq!(cache.disk_stats().pinned_bytes, 44 + 44 + 42);
    assert_eq!(cache.gc(0), Evicted::default());
    assert_eq!(
        cache.bytes_on_disk(
            &link,
            &rev,
            &cache.layout(&link, &rev, 10).await.expect("layout")
        ),
        10
    );
    store.flush().await.expect("flush");
    let pinned: i64 = store
        .read(|conn| {
            conn.query_row(
                "SELECT count(*) FROM cache_blocks WHERE pin = 1",
                [],
                |row| row.get(0),
            )
        })
        .expect("read");
    assert_eq!(pinned, 3);
    // Let go, they count against the cap again and are evicted to 80 %.
    cache.set_pinned_revisions(&HashSet::new());
    let stats = cache.disk_stats();
    assert_eq!(stats.pinned_bytes, 0);
    assert!(stats.bytes <= 80, "{stats:?}");
}

#[tokio::test]
async fn a_lowered_cap_applies_when_the_cache_opens() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let (cache, store, link, rev) = setup(dir.path(), 1 << 20).await;
        fetch_all(&cache, &link, &rev).await;
        store.flush().await.expect("flush");
    }
    let (cache, _store, _, _) = setup(dir.path(), 100).await;
    assert!(cache.usage().0 <= 80, "{:?}", cache.usage());
}

#[tokio::test]
async fn gc_reaches_its_target_and_wipe_keeps_other_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (cache, _store, link, rev) = setup(dir.path(), 1 << 20).await;
    let keys = fetch_all(&cache, &link, &rev).await;
    let snapshot = dir.path().join("snapshot");
    std::fs::write(&snapshot, b"other").expect("write");
    let (other_link, other_rev) = (LinkId("other".into()), RevisionId("r".into()));
    cache
        .seed(&other_link, &other_rev, &snapshot, 4)
        .await
        .expect("seed");
    let other = BlockKey::new(&other_link, &other_rev, 1);
    assert!(on_disk(&cache, &other));

    let before = cache.disk_stats().bytes;
    let evicted = cache.gc(before - 1);
    assert_eq!(evicted.blocks, 1);
    assert!(cache.disk_stats().bytes < before);
    assert_eq!(cache.gc(1 << 30), Evicted::default());

    let wiped = cache.wipe(Some(&HashSet::from([link.clone()])));
    assert!(wiped.blocks >= 2);
    assert!(keys.iter().all(|key| !on_disk(&cache, key)));
    assert!(keys.iter().all(|key| !in_ram(&cache, key)));
    assert!(on_disk(&cache, &other));
    cache.wipe(None);
    assert_eq!(cache.usage(), (0, 0));
    assert!(!cache.disk.path(&other).exists());
}

/// Bytes of one 4-byte block on disk.
const SEALED_BLOCK: u64 = 4 + SEAL_OVERHEAD as u64;
/// 100 such blocks: the upper mark is 90 of them, the lower 80.
const SHARED_CAP: u64 = 100 * SEALED_BLOCK;

/// A cache in its own `dir` on `budget`.
async fn on_budget(dir: &Path, budget: &Arc<DiskBudget>, keeps_half: bool) -> Arc<BlockCache> {
    std::fs::create_dir_all(dir).expect("mkdir");
    let shared = SharedCap {
        budget: Arc::clone(budget),
        keeps_half,
    };
    let pool = Pool::new(16 << 20).expect("pool");
    setup_shared(dir, 1 << 30, b"0123", pool, Some(shared))
        .await
        .0
}

/// Stores `blocks` new 4-byte blocks of files `name-<n>`, one by one,
/// and checks after each that the budget stays under its upper mark.
async fn fill(cache: &BlockCache, dir: &Path, name: &str, blocks: u64) {
    let snapshot = dir.join(format!("snapshot-{name}"));
    std::fs::write(&snapshot, b"abcd").expect("write");
    let budget = Arc::clone(&cache.seat.budget);
    for block in 0..blocks {
        let (link, rev) = (LinkId(format!("{name}-{block}")), RevisionId("r".into()));
        cache.seed(&link, &rev, &snapshot, 4).await.expect("seed");
        assert!(
            budget.used() <= budget.cap() / 10 * 9,
            "{} over the upper mark",
            budget.used()
        );
    }
}

#[tokio::test]
async fn two_caches_on_one_budget_stay_under_the_total() {
    let dir = tempfile::tempdir().expect("tempdir");
    let budget = DiskBudget::new(SHARED_CAP);
    let my_files = on_budget(&dir.path().join("a"), &budget, true).await;
    let tree = on_budget(&dir.path().join("b"), &budget, false).await;
    for round in 0..4 {
        fill(&my_files, dir.path(), &format!("a{round}"), 30).await;
        fill(&tree, dir.path(), &format!("b{round}"), 30).await;
    }
    let (mine, theirs) = (my_files.disk_stats(), tree.disk_stats());
    assert_eq!(mine.profile_bytes, mine.bytes + theirs.bytes);
    assert_eq!(mine.max_bytes, SHARED_CAP);
    assert_eq!(mine.caches, 2);
    assert!(mine.profile_bytes <= SHARED_CAP / 10 * 9);
    assert!(mine.bytes > 0 && theirs.bytes > 0);
}

#[tokio::test]
async fn further_trees_cannot_push_my_files_below_half() {
    let dir = tempfile::tempdir().expect("tempdir");
    let budget = DiskBudget::new(SHARED_CAP);
    let my_files = on_budget(&dir.path().join("a"), &budget, true).await;
    let first = on_budget(&dir.path().join("b"), &budget, false).await;
    let second = on_budget(&dir.path().join("c"), &budget, false).await;
    fill(&my_files, dir.path(), "a", 80).await;
    assert_eq!(my_files.disk_stats().bytes, 80 * SEALED_BLOCK);
    for round in 0..3 {
        fill(&first, dir.path(), &format!("b{round}"), 40).await;
        assert!(my_files.disk_stats().bytes >= SHARED_CAP / 2);
        fill(&second, dir.path(), &format!("c{round}"), 40).await;
        assert!(my_files.disk_stats().bytes >= SHARED_CAP / 2);
    }
    // The others asked My Files to shrink down to its floor, no further.
    assert_eq!(my_files.disk_stats().bytes, SHARED_CAP / 2);
    // My Files itself may still go below half.
    my_files.gc(0);
    assert_eq!(my_files.disk_stats().bytes, 0);
}

#[tokio::test]
async fn a_tree_uses_free_space() {
    let dir = tempfile::tempdir().expect("tempdir");
    let budget = DiskBudget::new(SHARED_CAP);
    let my_files = on_budget(&dir.path().join("a"), &budget, true).await;
    let tree = on_budget(&dir.path().join("b"), &budget, false).await;
    // Far over its share of half, with no one else using the cap.
    fill(&tree, dir.path(), "b", 85).await;
    assert_eq!(tree.disk_stats().bytes, 85 * SEALED_BLOCK);
    assert_eq!(my_files.disk_stats().profile_bytes, 85 * SEALED_BLOCK);
    // Over the upper mark it evicts its own blocks, down to the lower.
    fill(&tree, dir.path(), "more", 10).await;
    assert_eq!(budget.used(), tree.disk_stats().bytes);
    assert!(budget.used() <= SHARED_CAP / 10 * 9);
}

#[tokio::test]
async fn a_writer_under_its_share_makes_the_furthest_over_evict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let budget = DiskBudget::new(SHARED_CAP);
    let my_files = on_budget(&dir.path().join("a"), &budget, true).await;
    let big = on_budget(&dir.path().join("b"), &budget, false).await;
    let small = on_budget(&dir.path().join("c"), &budget, false).await;
    // Shares: My Files 50 blocks, the others 25 each.
    fill(&big, dir.path(), "b", 60).await;
    fill(&small, dir.path(), "c", 28).await;
    fill(&my_files, dir.path(), "a", 10).await;
    assert_eq!(my_files.disk_stats().bytes, 10 * SEALED_BLOCK);
    assert_eq!(
        small.disk_stats().bytes,
        28 * SEALED_BLOCK,
        "less far over its share"
    );
    assert!(big.disk_stats().bytes < 60 * SEALED_BLOCK);
}

#[tokio::test]
async fn a_tree_that_shrinks_frees_room_for_the_others() {
    let dir = tempfile::tempdir().expect("tempdir");
    let budget = DiskBudget::new(SHARED_CAP);
    let my_files = on_budget(&dir.path().join("a"), &budget, true).await;
    let tree = on_budget(&dir.path().join("b"), &budget, false).await;
    fill(&tree, dir.path(), "b", 85).await;
    tree.wipe(None);
    assert_eq!(budget.used(), 0);
    // All of the room is My Files' now: nothing is evicted below the mark.
    fill(&my_files, dir.path(), "a", 85).await;
    assert_eq!(my_files.disk_stats().bytes, 85 * SEALED_BLOCK);
    // A cache that closes leaves the budget, with its bytes.
    fill(&tree, dir.path(), "again", 5).await;
    assert_eq!(budget.caches(), 2);
    drop(tree);
    assert_eq!(budget.caches(), 1);
    assert_eq!(budget.used(), my_files.disk_stats().bytes);
}

#[tokio::test]
async fn a_cache_alone_keeps_its_own_cap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (cache, _store, _link, _rev) = setup(dir.path(), SHARED_CAP).await;
    fill(&cache, dir.path(), "a", 95).await;
    let stats = cache.disk_stats();
    assert_eq!((stats.caches, stats.max_bytes), (1, SHARED_CAP));
    assert_eq!(stats.profile_bytes, stats.bytes);
    assert!(stats.bytes <= SHARED_CAP / 10 * 9);
}

/// Makes a file look older than the sweep's grace.
fn backdate(path: &Path) {
    let file = std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open");
    file.set_modified(SystemTime::now() - Duration::from_secs(60))
        .expect("set mtime");
}

#[tokio::test]
async fn the_sweep_removes_orphan_files_and_dangling_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (cache, store, link, rev) = setup(dir.path(), 1 << 20).await;
    let keys = fetch_all(&cache, &link, &rev).await;
    store.flush().await.expect("flush");
    // A file without a row, a crashed write, and a file too new to judge.
    let blocks = dir.path().join("blocks/ab");
    std::fs::create_dir_all(&blocks).expect("mkdir");
    let (orphan, temp, fresh) = (
        blocks.join("ab00"),
        blocks.join(".tmp-x"),
        blocks.join("ab01"),
    );
    for path in [&orphan, &temp, &fresh] {
        std::fs::write(path, b"x").expect("write");
    }
    backdate(&orphan);
    backdate(&temp);
    // A row whose file is gone.
    std::fs::remove_file(cache.disk.path(&keys[1])).expect("remove");

    let swept = cache.sweep().expect("sweep");
    assert_eq!(
        swept,
        Swept {
            orphan_files: 2,
            dangling_rows: 1
        }
    );
    assert!(!orphan.exists() && !temp.exists());
    assert!(fresh.exists());
    assert!(!cache.disk.index.lock().entries.contains_key(&keys[1]));
    assert!(on_disk(&cache, &keys[0]) && on_disk(&cache, &keys[2]));
    store.flush().await.expect("flush");
    let rows: i64 = store
        .read(|conn| conn.query_row("SELECT count(*) FROM cache_blocks", [], |row| row.get(0)))
        .expect("read");
    assert_eq!(rows, 2);
    // Bookkeeping agrees after the dangling row went.
    assert_eq!(cache.sweep().expect("sweep"), Swept::default());
}

#[tokio::test]
async fn a_short_block_is_an_integrity_error_and_not_cached() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (cache, _store, link, rev) = setup(dir.path(), 1 << 20).await;
    let key = BlockKey {
        link,
        rev,
        index: 3,
    };
    let error = cache
        .get(&key, 4, Priority::Interactive)
        .await
        .expect_err("short");
    assert_eq!(error.errno(), nix::libc::EIO);
    assert!(!cache.contains(&key));
}

fn counter(counter: &std::sync::atomic::AtomicU64) -> u64 {
    counter.load(std::sync::atomic::Ordering::Relaxed)
}

fn in_ram(cache: &BlockCache, key: &BlockKey) -> bool {
    cache.ram.lock().entries.contains(key)
}

#[tokio::test]
async fn held_blocks_keep_their_slots_and_the_hot_path_falls_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = Pool::with_slots([2, 0, 0]).expect("pool");
    let (cache, _store, link, rev, _) =
        setup_pooled(dir.path(), 1 << 20, b"0123456789", pool).await;
    let key = |index| BlockKey::new(&link, &rev, index);
    for (index, len) in [(1, 4), (2, 4), (3, 2)] {
        cache
            .get(&key(index), len, Priority::Interactive)
            .await
            .expect("get");
    }
    // Two slots: block 3 took block 1's.
    assert!(!in_ram(&cache, &key(1)));
    let held = (
        cache.get_local(&key(2)).expect("2"),
        cache.get_local(&key(3)).expect("3"),
    );
    assert_eq!(cache.pool_usage()[0].in_use, 2);
    // Evicting blocks 2 and 3 frees no slot while they are held, so block
    // 1 comes from the heap and is not kept.
    assert_eq!(cache.get_local(&key(1)).expect("1"), &b"0123"[..]);
    assert_eq!(counter(&cache.metrics.cache_pool_fallbacks), 1);
    assert!(!in_ram(&cache, &key(1)) && !in_ram(&cache, &key(2)));
    assert_eq!(cache.ram.lock().bytes, 0);
    assert_eq!(&held.0[..], b"4567");
    drop(held);
    assert_eq!(cache.pool_usage()[0].in_use, 0);
    assert_eq!(cache.get_local(&key(1)).expect("1"), &b"0123"[..]);
    assert_eq!(counter(&cache.metrics.cache_pool_fallbacks), 1);
    assert!(in_ram(&cache, &key(1)));
    assert_eq!(cache.ram.lock().bytes, cache.pool.slot_size(0) as u64);
}

#[tokio::test]
async fn a_download_waits_for_a_held_slot() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = Pool::with_slots([1, 0, 0]).expect("pool");
    let (cache, _store, link, rev, _) =
        setup_pooled(dir.path(), 1 << 20, b"0123456789", pool).await;
    let key = |index| BlockKey::new(&link, &rev, index);
    let held = cache
        .get(&key(1), 4, Priority::Interactive)
        .await
        .expect("get");
    let download = tokio::spawn({
        let cache = Arc::clone(&cache);
        let key = key(2);
        async move { cache.get(&key, 4, Priority::Interactive).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!download.is_finished());
    drop(held);
    let data = download.await.expect("join").expect("get");
    assert_eq!(&data[..], b"4567");
    assert_eq!(counter(&cache.metrics.cache_pool_fallbacks), 0);
    assert!(in_ram(&cache, &key(2)));
}

#[tokio::test]
async fn seed_round_trips_through_the_disk_cache() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (cache, _store, _, _) = setup(dir.path(), 1 << 20).await;
    let snapshot = dir.path().join("snapshot");
    std::fs::write(&snapshot, b"seeded content!").expect("write");
    let (link, rev) = (LinkId("seeded".into()), RevisionId("r1".into()));
    cache.seed(&link, &rev, &snapshot, 4).await.expect("seed");
    assert!(cache.pool_usage().iter().all(|class| class.in_use == 0));
    let layout = cache.layout_local(&link, &rev, 15).expect("layout");
    assert_eq!(layout.blocks.len(), 4);
    let mut read = Vec::new();
    for span in &layout.blocks {
        let key = BlockKey::new(&link, &rev, span.index);
        read.extend_from_slice(&cache.get_local(&key).expect("seeded"));
    }
    assert_eq!(read, b"seeded content!");
    assert_eq!(counter(&cache.metrics.cache_misses), 0);
    assert_eq!(counter(&cache.metrics.cache_disk_hits), 4);
}

// ---- the bulk-read guard's blocks (D111, D120) ----

/// A scan seat for `cache` on a guard whose scan blocks may fill `share`
/// percent of `cap`; the guard's evictions go to the cache.
fn scan_seat(cache: &Arc<BlockCache>, cap: u64, share: u8) -> Arc<crate::scan::ScanGuard> {
    let config = crate::config::ScanConfig {
        cache_share: share,
        ..Default::default()
    };
    let guard = crate::scan::ScanGuard::new(&config, cap);
    let evicting = Arc::downgrade(cache);
    cache.set_scan_seat(guard.join(Arc::new(move |key: &BlockKey| {
        if let Some(cache) = evicting.upgrade() {
            cache.evict_scan_block(key);
        }
    })));
    guard
}

/// A read a user waits on that joins a scan download still waiting for
/// the session's scan permit raises it to interactive priority instead of
/// waiting behind the scan queue; the block is then part of the working
/// set, no scan block.
#[tokio::test]
async fn an_interactive_read_raises_a_scan_download_waiting_for_its_permit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (cache, _store, link, rev) = setup(dir.path(), 1 << 20).await;
    let guard = scan_seat(&cache, 1 << 20, 10);
    cache.layout(&link, &rev, 10).await.expect("layout");
    // Another scan download holds the scan permit.
    let other_scan = Arc::clone(&cache.downloads.scan)
        .acquire_owned()
        .await
        .expect("permit");
    let key = BlockKey::new(&link, &rev, 1);
    let scan = tokio::spawn({
        let cache = Arc::clone(&cache);
        let key = key.clone();
        async move { cache.get(&key, 4, Priority::Scan).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(cache.in_flight(&key), "the scan download waits");
    let read = tokio::time::timeout(
        Duration::from_secs(5),
        cache.get(&key, 4, Priority::Interactive),
    )
    .await
    .expect("the read waited behind the scan queue")
    .expect("get");
    assert_eq!(&read[..], b"0123");
    assert_eq!(&scan.await.expect("join").expect("get")[..], b"0123");
    assert_eq!(counter(&cache.metrics.cache_misses), 1, "one download");
    assert_eq!(guard.held(), 0, "a block a user waited for");
    // A scan download nobody joins still waits for the scan permit.
    let second = BlockKey::new(&link, &rev, 2);
    let waiting = tokio::spawn({
        let cache = Arc::clone(&cache);
        let key = second.clone();
        async move { cache.get(&key, 4, Priority::Scan).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!waiting.is_finished());
    drop(other_scan);
    waiting.await.expect("join").expect("get");
    assert_eq!(guard.held(), SEALED_BLOCK, "a scan block");
}

/// Scan blocks that leave the disk by another path than the share's
/// eviction (`cache gc`, a wipe, the cap) are no scan blocks any more, and
/// neither is one another read downloads again: the share counts only the
/// scan blocks on disk, and never evicts a working-set block as an old
/// scan block.
#[tokio::test]
async fn scan_blocks_gone_elsewhere_or_downloaded_again_are_forgotten() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (cache, _store, link, rev) = setup(dir.path(), 1 << 20).await;
    // Room for two 4-byte scan blocks.
    let guard = scan_seat(&cache, 100 * SEALED_BLOCK, 2);
    cache.layout(&link, &rev, 10).await.expect("layout");
    let keys: Vec<BlockKey> = (1..=2)
        .map(|index| BlockKey::new(&link, &rev, index))
        .collect();
    for key in &keys {
        cache.get(key, 4, Priority::Scan).await.expect("get");
    }
    assert_eq!(guard.held(), 2 * SEALED_BLOCK);
    // `cache gc` takes them: no scan block is left.
    assert_eq!(cache.gc(0).blocks, 2);
    assert_eq!(guard.held(), 0, "evicted by gc, still counted");
    assert!(!cache.scan_seat().expect("seat").any_blocks());
    // A wipe too (of a block downloaded again: gc left it in RAM).
    drop(cache.ram.lock().remove(&keys[0]));
    cache.get(&keys[0], 4, Priority::Scan).await.expect("get");
    assert_eq!(guard.held(), SEALED_BLOCK);
    cache.wipe(None);
    assert_eq!(guard.held(), 0, "wiped, still counted");
    // Brought in by a scan, then gone from the cache without the guard
    // hearing of it, and downloaded again by a read a user waits on: part
    // of the working set.
    cache.get(&keys[0], 4, Priority::Scan).await.expect("get");
    cache.disk.index.lock().remove(&keys[0]);
    drop(cache.ram.lock().remove(&keys[0]));
    cache
        .get(&keys[0], 4, Priority::Interactive)
        .await
        .expect("get");
    assert_eq!(guard.held(), 0, "downloaded again by a user's read");
    // So the scan blocks that follow never take it as the oldest.
    let third = BlockKey::new(&link, &rev, 3);
    cache.get(&keys[1], 4, Priority::Scan).await.expect("get");
    cache.get(&third, 2, Priority::Scan).await.expect("get");
    assert!(on_disk(&cache, &keys[0]), "the working set stays");
    assert!(guard.held() <= guard.limit());
    // A pin of their revision: pinned blocks are outside the share.
    assert!(guard.held() > 0);
    cache.set_pinned_revisions(&HashSet::from([(link.clone(), rev.clone())]));
    assert_eq!(guard.held(), 0, "pinned, still counted");
}
