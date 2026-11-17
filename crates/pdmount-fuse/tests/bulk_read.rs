//! The bulk-read guard on a mount (DESIGN.md §6.4, D081, D111): a burst of
//! small files read in one tree of a mount must not slow the interactive
//! reads of another, over a simulated network (`NetProfile`). Skipped when
//! FUSE is not usable (no /dev/fuse or fusermount3).

use pdmount_backend::fake::{FakeBackend, FakeRole, NetProfile};
use pdmount_backend::{Backend, FileMeta, LinkId, UploadSource};
use pdmount_core::Config;
use pdmount_core::conflicts::system_clock;
use pdmount_core::device::{DeviceIdentity, Uuid};
use pdmount_core::engine::{Engine, EngineOptions};
use pdmount_core::scan::ScanGuard;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Small blocks and a slow link, so the network, not the debug build's
/// cipher, makes the latency.
const BLOCK: usize = 256 << 10;
/// Blocks of each large file; one interactive read per block.
const BIG_BLOCKS: usize = 6;
const BURST_FILES: usize = 120;
const BURST_SIZE: usize = 128 << 10;
const BURST_THREADS: usize = 8;

fn fuse_available() -> bool {
    let device = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .is_ok();
    let fusermount = Command::new("fusermount3")
        .arg("--version")
        .output()
        .is_ok();
    device && fusermount
}

fn upload(
    runtime: &tokio::runtime::Runtime,
    backend: &FakeBackend,
    parent: &LinkId,
    name: &str,
    len: usize,
) {
    let content: Vec<u8> = (0..len).map(|i| (i * 7 % 251) as u8).collect();
    runtime
        .block_on(backend.upload_new(
            parent,
            name,
            UploadSource::Bytes(bytes::Bytes::from(content)),
            FileMeta {
                mtime: std::time::SystemTime::now(),
                mode: Some(0o644),
                media_type: "application/octet-stream".into(),
                sha1: None,
            },
        ))
        .expect("upload");
}

struct Run {
    /// Latency of one read per block of a large file, with nothing else
    /// running.
    quiet: Vec<Duration>,
    /// The same, of another large file, opened before the burst and read
    /// while it runs.
    during: Vec<Duration>,
    burst_files: usize,
    scan_reads: u64,
    held: u64,
    limit: u64,
}

fn median(samples: &[Duration]) -> Duration {
    let mut sorted = samples.to_vec();
    sorted.sort();
    sorted[sorted.len() / 2]
}

/// Opens `path` without kernel readahead, so each read below is one FUSE
/// read and the engine sees no sequential run.
fn open_random(path: &std::path::Path) -> std::fs::File {
    let file = std::fs::File::open(path).expect("open");
    nix::fcntl::posix_fadvise(
        &file,
        0,
        0,
        nix::fcntl::PosixFadviseAdvice::POSIX_FADV_RANDOM,
    )
    .expect("fadvise");
    file
}

/// One read of 4 KiB at the start of each block of `file`.
fn block_reads(file: &std::fs::File) -> Vec<Duration> {
    let mut buf = vec![0u8; 4096];
    (0..BIG_BLOCKS)
        .map(|block| {
            let started = Instant::now();
            file.read_exact_at(&mut buf, (block * BLOCK) as u64)
                .expect("read");
            started.elapsed()
        })
        .collect()
}

/// A mount with two trees, "Mine" on the drive's own volume and "Team" on
/// another user's, over one session's guard (`scan.opens` as given).
/// "Mine" holds two large files, "Team" the burst's small ones.
fn run(scan_opens: u32) -> Option<Run> {
    if !fuse_available() {
        eprintln!("skipped: FUSE is not available");
        return None;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let temp = tempfile::tempdir().expect("tempdir");
    let mnt = temp.path().join("mnt");
    std::fs::create_dir_all(&mnt).expect("mkdir");
    let backend = FakeBackend::new();
    backend.set_block_size(BLOCK);
    let mine = backend.add_root("Mine", None, None).expect("root");
    let team = backend
        .add_root("Team", Some("owner@example.test"), Some(FakeRole::Editor))
        .expect("share");
    let mine_tree = backend.tree(&mine.volume, mine.root.clone());
    let team_tree = backend.tree(&team.volume, team.root.clone());
    for name in ["quiet.bin", "during.bin"] {
        upload(&runtime, &mine_tree, &mine.root, name, BIG_BLOCKS * BLOCK);
    }
    for n in 0..BURST_FILES {
        upload(
            &runtime,
            &team_tree,
            &team.root,
            &format!("s{n:03}"),
            BURST_SIZE,
        );
    }
    // 20 ms round trips, 1 MB/s down: a large file's block takes about
    // 280 ms, a burst file about 150 ms.
    backend.set_profile(NetProfile {
        rtt: Duration::from_millis(20),
        up_bytes_per_sec: None,
        down_bytes_per_sec: Some(1_000_000),
    });

    let mut config = Config::default();
    config.scan.opens = scan_opens;
    let guard = ScanGuard::new(&config.scan, 64 << 20);
    let _entered = runtime.enter();
    let mut engines: Vec<Arc<Engine>> = Vec::new();
    let mut trees = Vec::new();
    for (name, tree) in [("Mine", mine_tree), ("Team", team_tree)] {
        let mut options = EngineOptions::in_dir(
            &temp.path().join("state").join(name),
            config.clone(),
            DeviceIdentity {
                id: Uuid::from_u128(1),
                name: "bulk-read-test".into(),
            },
            system_clock(),
        );
        options.write_gate = false;
        options.health = engines.first().map(|engine| Arc::clone(engine.health()));
        options.scan = Some(Arc::clone(&guard));
        let (engine, invalidations) = runtime
            .block_on(Engine::open(Arc::new(tree), options))
            .expect("engine");
        trees.push(pdmount_fuse::MountTree {
            name: name.to_owned(),
            engine: Arc::clone(&engine),
            invalidations,
            volume_use: None,
            place: pdmount_fuse::TreePlace::Folder,
        });
        engines.push(engine);
    }
    let mounted = pdmount_fuse::mount_trees(trees, &mnt).expect("mount");

    // Listings first, so only block reads are timed.
    let burst: Vec<PathBuf> = {
        let mut names: Vec<PathBuf> = std::fs::read_dir(mnt.join("Team"))
            .expect("list")
            .map(|entry| entry.expect("entry").path())
            .collect();
        names.sort();
        names
    };
    assert_eq!(burst.len(), BURST_FILES);
    let quiet_file = open_random(&mnt.join("Mine/quiet.bin"));
    let quiet = block_reads(&quiet_file);
    // Opened before the burst: interactive.
    let during_file = open_random(&mnt.join("Mine/during.bin"));

    let stop = Arc::new(AtomicBool::new(false));
    let next = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));
    let threads: Vec<_> = (0..BURST_THREADS)
        .map(|_| {
            let (stop, next, done, burst) = (
                Arc::clone(&stop),
                Arc::clone(&next),
                Arc::clone(&done),
                burst.clone(),
            );
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let at = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = burst.get(at) else { break };
                    let data = std::fs::read(path).expect("read a burst file");
                    assert_eq!(data.len(), BURST_SIZE);
                    done.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    // Under way: the scan has begun (or, without the guard, as many files
    // as it takes to begin one were opened).
    let waited = Instant::now();
    while next.load(Ordering::Relaxed) < 12 + BURST_THREADS {
        assert!(
            waited.elapsed() < Duration::from_secs(30),
            "the burst stalled"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let during = block_reads(&during_file);
    stop.store(true, Ordering::Relaxed);
    for thread in threads {
        thread.join().expect("burst thread");
    }
    let team_engine = &engines[1];
    let result = Run {
        quiet,
        during,
        burst_files: done.load(Ordering::Relaxed),
        scan_reads: team_engine.metrics().scan_reads.load(Ordering::Relaxed),
        held: guard.held(),
        limit: guard.limit(),
    };
    drop((quiet_file, during_file));
    mounted.unmount().expect("unmount");
    for engine in &engines {
        runtime.block_on(engine.shutdown()).expect("shutdown");
    }
    Some(result)
}

/// D111: while many small files of one tree are read at once, as a
/// thumbnailer does, the interactive reads of a large file in another tree
/// keep about their latency; without the guard the burst slows them.
#[test]
fn interactive_reads_keep_their_latency_during_a_burst_of_opens() {
    let Some(guarded) = run(Config::default().scan.opens) else {
        return;
    };
    let Some(unguarded) = run(0) else {
        return;
    };
    let (quiet, during) = (median(&guarded.quiet), median(&guarded.during));
    let unguarded_during = median(&unguarded.during);
    eprintln!(
        "block read medians: quiet {quiet:?}; during the burst {during:?} with the guard \
         ({} files, {} scan reads), {unguarded_during:?} without ({} files)",
        guarded.burst_files, guarded.scan_reads, unguarded.burst_files
    );
    assert!(guarded.scan_reads > 0, "the burst became a scan");
    assert_eq!(unguarded.scan_reads, 0);
    assert!(
        during <= quiet + quiet / 2 + Duration::from_millis(50),
        "interactive reads slowed from {quiet:?} to {during:?} during a scan"
    );
    assert!(
        unguarded_during > during,
        "the burst did not slow reads without the guard ({unguarded_during:?}): the test measures nothing"
    );
    assert!(
        guarded.held <= guarded.limit,
        "{} bytes of scan blocks, over the share of {}",
        guarded.held,
        guarded.limit
    );
}
