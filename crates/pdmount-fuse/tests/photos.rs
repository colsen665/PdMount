//! A mounted Photos tree over the fake backend (M5.13, D113): years and
//! months, a read with the block cache, the thumbnails of an opened month
//! in a temporary thumbnail cache (M5.13b, D115), EROFS on every change,
//! and a new photo after an event. Skipped when FUSE is not usable (no
//! /dev/fuse or fusermount3).

use bytes::Bytes;
use pdmount_backend::fake::FakeBackend;
use pdmount_backend::photos::tag;
use pdmount_backend::{LinkId, UploadSource};
use pdmount_core::Config;
use pdmount_core::cache::CacheOptions;
use pdmount_core::photos::thumbnails::{SIZES, cache_name, file_uri};
use pdmount_core::photos::{PhotosOptions, PhotosTree, ThumbnailOptions, utc_month};
use pdmount_core::units::HumanDuration;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Mid-month, so no time zone moves them.
const MARCH_2023: i64 = 1_678_881_600;
const JULY_2023: i64 = 1_689_422_400;
const MAY_2021: i64 = 1_621_080_000;

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

fn content(name: &str) -> Vec<u8> {
    // Two blocks of the fake's 4 MiB for the big one.
    let size = if name.starts_with("BIG") {
        5 << 20
    } else {
        3000
    };
    let seed = name
        .bytes()
        .fold(7u8, |a, b| a.wrapping_mul(31).wrapping_add(b));
    (0..size)
        .map(|i| (i as u8).wrapping_mul(seed) ^ seed)
        .collect()
}

async fn add(
    fake: &FakeBackend,
    name: &str,
    capture: i64,
    tags: Vec<u8>,
    main: Option<LinkId>,
) -> LinkId {
    fake.add_photo(
        name,
        UploadSource::Bytes(Bytes::from(content(name))),
        capture,
        tags,
        main,
    )
    .await
    .expect("add photo")
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .into_string()
                .expect("utf8")
        })
        .collect();
    names.sort();
    names
}

fn wait_for(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < limit, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn expect_erofs(what: &str, result: std::io::Result<impl std::fmt::Debug>) {
    match result {
        Ok(value) => panic!("{what} succeeded: {value:?}"),
        Err(error) => assert_eq!(
            error.raw_os_error(),
            Some(nix::libc::EROFS),
            "{what}: {error}"
        ),
    }
}

#[test]
fn photos_by_year_and_month_read_only_with_events() {
    if !fuse_available() {
        eprintln!("skipped: FUSE is not available");
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let temp = tempfile::tempdir().expect("tempdir");
    let mnt = temp.path().join("mnt");
    std::fs::create_dir_all(&mnt).expect("mkdir");
    let fake = FakeBackend::new();
    let fake_dir = temp.path().join("state");
    // The thumbnail cache of a temporary `XDG_CACHE_HOME`.
    let thumbnails = temp.path().join("xdg-cache/thumbnails");
    let tree = runtime.block_on(async {
        add(&fake, "IMG_0001.jpg", MARCH_2023, Vec::new(), None).await;
        add(&fake, "BIG_0002.jpg", MARCH_2023 + 60, Vec::new(), None).await;
        let still = add(
            &fake,
            "IMG_0003.HEIC",
            MARCH_2023 + 120,
            vec![tag::LIVE_PHOTO],
            None,
        )
        .await;
        add(
            &fake,
            "IMG_0003.MOV",
            MARCH_2023 + 121,
            vec![tag::VIDEO],
            Some(still),
        )
        .await;
        add(&fake, "IMG_0004.jpg", JULY_2023, Vec::new(), None).await;
        add(&fake, "IMG_0005.jpg", MAY_2021, Vec::new(), None).await;
        let volume = fake.photos_volume_now().expect("a Photos volume");
        let mut config = Config::default();
        // Events within the test's patience (the config's floor is 30 s).
        config.sync.poll_interval = HumanDuration(Duration::from_secs(1));
        let options = PhotosOptions {
            volume: volume.volume.clone(),
            root: volume.root.clone(),
            state_db: fake_dir.join("state.db"),
            cache: CacheOptions {
                dir: fake_dir.join("blocks"),
                master_key: Some([7; 32]),
                ram_bytes: 16 << 20,
                disk_bytes: 64 << 20,
                shared: None,
                downloads: None,
            },
            config,
            health: None,
            placer: utc_month,
            thumbnails: Some(ThumbnailOptions {
                dir: thumbnails.clone(),
                mountpoint: mnt.clone(),
            }),
            scan: None,
            limits: None,
        };
        let backend = Arc::new(fake.tree(&volume.volume, volume.root.clone()));
        PhotosTree::open(backend, Arc::new(fake.clone()), options)
            .await
            .expect("open")
    });
    let (tree, invalidations) = tree;
    tree.start();
    let mounted = pdmount_fuse::mount_read_only(
        Arc::clone(&tree) as Arc<dyn pdmount_core::readonly::ReadOnlyTree>,
        invalidations,
        &mnt,
    )
    .expect("mount");

    // Years and months from the timeline; the first listing waits for it.
    assert_eq!(names(&mnt), ["2021", "2023"]);
    assert_eq!(names(&mnt.join("2023")), ["03", "07"]);
    assert_eq!(names(&mnt.join("2021")), ["05"]);
    let march = mnt.join("2023/03");
    assert_eq!(
        std::fs::metadata(&march).expect("stat").mode() & 0o7777,
        0o555
    );
    // Names when the month is opened; the Live Photo's video beside it.
    wait_for("March's names", Duration::from_secs(20), || {
        names(&march).len() == 4
    });
    assert_eq!(
        names(&march),
        [
            "BIG_0002.jpg",
            "IMG_0001.jpg",
            "IMG_0003.HEIC",
            "IMG_0003.MOV"
        ]
    );

    // Opening the month wrote its thumbnails, with the URI and mtime the
    // mount reports, so a file manager takes them.
    let thumbnail = |size: &str, name: &str| {
        let uri = file_uri(&march.join(name));
        thumbnails.join(size).join(cache_name(&uri))
    };
    wait_for("March's thumbnails", Duration::from_secs(30), || {
        names(&march)
            .iter()
            .all(|name| SIZES.iter().all(|(size, _)| thumbnail(size, name).exists()))
    });
    for name in names(&march) {
        let mtime = std::fs::metadata(march.join(&name)).expect("stat").mtime();
        for (size, side) in SIZES {
            let file = std::fs::File::open(thumbnail(size, &name)).expect("thumbnail");
            assert_eq!(file.metadata().expect("stat").mode() & 0o777, 0o600);
            let reader = png::Decoder::new(std::io::BufReader::new(file))
                .read_info()
                .expect("a PNG");
            let info = reader.info();
            let text = |key: &str| {
                info.uncompressed_latin1_text
                    .iter()
                    .find(|chunk| chunk.keyword == key)
                    .map(|chunk| chunk.text.clone())
            };
            assert_eq!(text("Thumb::URI"), Some(file_uri(&march.join(&name))));
            assert_eq!(text("Thumb::MTime"), Some(mtime.to_string()));
            // The fake's 512 x 384, scaled down.
            assert_eq!((info.width, info.height), (side, side * 3 / 4), "{size}");
        }
    }
    assert_eq!(
        std::fs::metadata(thumbnails.join("normal"))
            .expect("stat")
            .mode()
            & 0o777,
        0o700
    );
    // July's month was not opened: no thumbnail of it.
    let july_uri = file_uri(&mnt.join("2023/07/IMG_0004.jpg"));
    assert!(
        !thumbnails
            .join("normal")
            .join(cache_name(&july_uri))
            .exists()
    );

    // Reads through the block cache, across blocks, with verification.
    let big = march.join("BIG_0002.jpg");
    let meta = std::fs::metadata(&big).expect("stat");
    assert_eq!(meta.len(), 5 << 20);
    assert_eq!(meta.mode() & 0o7777, 0o444);
    assert_eq!(meta.mtime(), MARCH_2023 + 60);
    assert!(std::fs::read(&big).expect("read") == content("BIG_0002.jpg"));
    assert_eq!(
        std::fs::read(march.join("IMG_0003.MOV")).expect("read"),
        content("IMG_0003.MOV")
    );
    assert!(tree.cache_usage().0 >= 5 << 20, "the blocks are cached");

    // Every change is EROFS.
    let photo = march.join("IMG_0001.jpg");
    expect_erofs("create", std::fs::File::create(march.join("new.jpg")));
    expect_erofs("mkdir", std::fs::create_dir(march.join("dir")));
    expect_erofs("mkdir at the top", std::fs::create_dir(mnt.join("2030")));
    expect_erofs("unlink", std::fs::remove_file(&photo));
    expect_erofs("rmdir", std::fs::remove_dir(mnt.join("2021/05")));
    expect_erofs("rename", std::fs::rename(&photo, march.join("x.jpg")));
    expect_erofs(
        "open for writing",
        std::fs::OpenOptions::new().write(true).open(&photo),
    );
    expect_erofs(
        "truncate",
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&photo),
    );
    expect_erofs(
        "chmod",
        std::fs::set_permissions(&photo, std::fs::Permissions::from_mode(0o644)),
    );
    expect_erofs(
        "symlink",
        std::os::unix::fs::symlink("x", march.join("link")),
    );
    expect_erofs("link", std::fs::hard_link(&photo, march.join("hard")));
    assert_eq!(
        std::fs::read(&photo).expect("read"),
        content("IMG_0001.jpg")
    );

    // A new photo arrives with its event, in a month never opened.
    runtime.block_on(add(&fake, "IMG_0006.jpg", JULY_2023 + 60, Vec::new(), None));
    let july = mnt.join("2023/07");
    wait_for("the new photo", Duration::from_secs(20), || {
        names(&july).contains(&"IMG_0006.jpg".to_owned())
    });
    assert_eq!(
        std::fs::read(july.join("IMG_0006.jpg")).expect("read"),
        content("IMG_0006.jpg")
    );
    // A deleted one goes, with its video.
    let still = fake.photo_named("IMG_0003.HEIC").expect("still");
    runtime.block_on(async {
        use pdmount_backend::Backend;
        fake.photos_tree()
            .expect("tree")
            .trash(&[still])
            .await
            .expect("trash")
    });
    wait_for("the deleted photo to go", Duration::from_secs(20), || {
        names(&march) == ["BIG_0002.jpg", "IMG_0001.jpg"]
    });

    mounted.unmount().expect("unmount");
    runtime.block_on(tree.shutdown()).expect("shutdown");
}
