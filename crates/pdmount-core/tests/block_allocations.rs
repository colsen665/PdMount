//! After warm-up, block reads make no block-sized allocation (D049): the
//! block buffers come from the pool reserved at startup. A counting global
//! allocator counts allocations of 64 KiB or more made on the test thread.

// A global allocator is an `unsafe impl`; it only forwards to `System`.
#![allow(unsafe_code)]

use bytes::Bytes;
use pdmount_backend::fake::FakeBackend;
use pdmount_backend::{Backend, FileMeta, LinkId, RevisionId, UploadSource};
use pdmount_core::Config;
use pdmount_core::cache::{BlockCache, BlockKey, CacheOptions, Priority};
use pdmount_core::conflicts::system_clock;
use pdmount_core::device::{DeviceIdentity, Uuid};
use pdmount_core::engine::{Engine, EngineOptions, OpenMode};
use pdmount_core::metrics::Metrics;
use pdmount_core::store::Store;
use pdmount_core::vfs::{Lookup, ROOT_INO};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::UNIX_EPOCH;

const BIG: usize = 64 << 10;

struct Counting;

static BIG_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

fn note(size: usize) {
    if size >= BIG && COUNTING.try_with(Cell::get).unwrap_or(false) {
        BIG_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: forwards every call to `System` unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Big allocations made on this thread by `f`.
fn big_allocations<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let before = BIG_ALLOCATIONS.load(Ordering::Relaxed);
    COUNTING.with(|counting| counting.set(true));
    let result = f();
    COUNTING.with(|counting| counting.set(false));
    (result, BIG_ALLOCATIONS.load(Ordering::Relaxed) - before)
}

const BLOCK: usize = 256 << 10;
/// One more than the 1 MiB class has slots in a 16 MiB pool, so cycling
/// through them misses the RAM cache every time.
const BLOCKS: u32 = 4;

/// A fake drive with one file `f` of [`BLOCKS`] blocks of [`BLOCK`] bytes.
async fn drive() -> (FakeBackend, LinkId, RevisionId, Vec<u8>) {
    let backend = FakeBackend::new();
    backend.set_block_size(BLOCK);
    let root = backend.root().await.expect("root").link_id;
    // Not a multiple of 256, so every block differs.
    let content: Vec<u8> = (0..BLOCK * BLOCKS as usize)
        .map(|i| (i % 251) as u8)
        .collect();
    let (link, rev) = backend
        .upload_new(
            &root,
            "f",
            UploadSource::Bytes(Bytes::from(content.clone())),
            FileMeta {
                mtime: UNIX_EPOCH,
                mode: None,
                media_type: "application/octet-stream".into(),
                sha1: None,
            },
        )
        .await
        .expect("upload");
    (backend, link, rev, content)
}

#[tokio::test]
async fn block_reads_after_warm_up_allocate_no_block_buffers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (backend, link, rev, content) = drive().await;
    let store = Arc::new(Store::open(&dir.path().join("state.db")).expect("store"));
    let metrics = Arc::new(Metrics::default());
    let cache = Arc::new(
        BlockCache::open(
            Arc::new(backend),
            Arc::clone(&store),
            Arc::clone(&metrics),
            CacheOptions {
                dir: dir.path().join("blocks"),
                master_key: Some([3; 32]),
                ram_bytes: 16 << 20,
                disk_bytes: 64 << 20,
                shared: None,
                downloads: None,
            },
            pdmount_core::staging::Protections::default(),
        )
        .expect("cache"),
    );
    let key = |index: u32| BlockKey::new(&link, &rev, index);
    let expected = |index: u32| {
        let start = (index as usize - 1) * BLOCK;
        &content[start..start + BLOCK]
    };
    // Warm-up: download every block, then one round of disk hits.
    cache
        .layout(&link, &rev, content.len() as u64)
        .await
        .expect("layout");
    for index in 1..=BLOCKS {
        cache
            .get(&key(index), BLOCK as u32, Priority::Interactive)
            .await
            .expect("download");
    }
    for index in 1..=BLOCKS {
        assert_eq!(
            cache.get_local(&key(index)).expect("local"),
            expected(index)
        );
    }
    let hits = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
    let (ram_before, disk_before) = (
        hits(&metrics.cache_ram_hits),
        hits(&metrics.cache_disk_hits),
    );

    let last = key(BLOCKS);
    let (_, allocated) = big_allocations(|| {
        for _ in 0..1000 {
            assert!(cache.get_local(&last).is_some());
        }
    });
    assert_eq!(allocated, 0, "RAM hits allocated block buffers");
    assert_eq!(hits(&metrics.cache_ram_hits) - ram_before, 1000);

    let keys: Vec<BlockKey> = (1..=BLOCKS).map(key).collect();
    let (correct, allocated) = big_allocations(|| {
        (0..100).all(|i| {
            let index = i % BLOCKS as usize;
            cache
                .get_local(&keys[index])
                .is_some_and(|data| data == expected(index as u32 + 1))
        })
    });
    assert!(correct, "a disk hit returned the wrong content");
    assert_eq!(allocated, 0, "disk hits allocated block buffers");
    assert_eq!(hits(&metrics.cache_disk_hits) - disk_before, 100);
    assert_eq!(hits(&metrics.cache_pool_fallbacks), 0);
}

#[tokio::test]
async fn engine_reads_after_warm_up_allocate_no_block_buffers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (backend, _, _, content) = drive().await;
    let (engine, _invalidations) = Engine::open(
        Arc::new(backend),
        EngineOptions::in_dir(
            dir.path(),
            Config::default(),
            DeviceIdentity {
                id: Uuid::from_bytes([9; 16]),
                name: "alloc".into(),
            },
            system_clock(),
        ),
    )
    .await
    .expect("engine");
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let Lookup::Found(attr) = engine.lookup(ROOT_INO, b"f").expect("lookup") else {
        panic!("f is missing");
    };
    let mode = OpenMode {
        read: true,
        write: false,
        truncate: false,
    };
    let (handle, _) = engine.open_file(attr.ino, mode).expect("open");
    const READ: u64 = 128 << 10;
    // Inside block `index` (1-based), and across the end of block 1.
    let inside = |index: u32| (u64::from(index) - 1) * BLOCK as u64 + 1000;
    let across = BLOCK as u64 - READ / 2;
    let read_local = |offset: u64| {
        let data = engine
            .read_local(handle, offset, READ as u32)
            .expect("read")
            .expect("local")
            .0;
        data == content[offset as usize..(offset + READ) as usize]
    };
    // Warm-up: download every block, then one round from disk.
    for index in 1..=BLOCKS {
        let data = engine
            .read(handle, inside(index), READ as u32)
            .await
            .expect("download");
        assert_eq!(data.len() as u64, READ);
    }
    assert!((1..=BLOCKS).all(|index| read_local(inside(index))));
    assert!(read_local(across));
    let metrics = Arc::clone(engine.metrics());
    let hits = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
    let disk_before = hits(&metrics.cache_disk_hits);

    let (correct, allocated) = big_allocations(|| (0..1000).all(|_| read_local(inside(2))));
    assert!(correct);
    assert_eq!(allocated, 0, "RAM hits allocated block buffers");

    // RAM holds blocks 4, 1 and 2 (most recent last): starting at 3,
    // every read misses it.
    let (correct, allocated) =
        big_allocations(|| (0..100).all(|i| read_local(inside((i + 2) % BLOCKS + 1))));
    assert!(correct);
    assert_eq!(allocated, 0, "disk hits allocated block buffers");
    assert_eq!(hits(&metrics.cache_disk_hits) - disk_before, 100);

    let (correct, allocated) = big_allocations(|| (0..100).all(|_| read_local(across)));
    assert!(correct);
    assert_eq!(allocated, 0, "reads across blocks allocated block buffers");
    assert_eq!(hits(&metrics.cache_pool_fallbacks), 0);
    engine.release_file(handle);
}
