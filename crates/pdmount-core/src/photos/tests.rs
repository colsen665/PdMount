//! Tests of the Photos tree: the model alone, then the tree over the fake
//! backend's Photos volume (its stages run by hand, without pacing).

use super::model::{Model, Photo, PhotoMeta, unique_name};
use super::*;
use bytes::Bytes;
use pdmount_backend::UploadSource;
use pdmount_backend::fake::{FakeBackend, FakeThumbnail};
use pdmount_backend::photos::{
    PhotosVolume, RelatedPhoto, TIMELINE_PAGE, ThumbnailRef, TimelineItem, tag,
};
use std::sync::atomic::AtomicUsize;

/// 2023-03-15 12:00 UTC and friends: mid-month, so no time zone moves them.
const MARCH_2023: i64 = 1_678_881_600;
const JULY_2023: i64 = 1_689_422_400;
const MAY_2021: i64 = 1_621_080_000;
const DAY: i64 = 86_400;

fn item(link: &str, capture: i64) -> TimelineItem {
    TimelineItem {
        link: LinkId::new(link),
        capture_time: capture,
        tags: Vec::new(),
        related: Vec::new(),
    }
}

fn meta(name: &str) -> Option<PhotoMeta> {
    Some(PhotoMeta {
        name: name.into(),
        size: 10,
        revision: RevisionId::new(format!("rev-{name}")),
    })
}

#[test]
fn years_and_months_come_from_the_timeline_alone() {
    let mut model = Model::new(utc_month);
    for (link, capture) in [
        ("a", MARCH_2023),
        ("b", MARCH_2023 + DAY),
        ("c", JULY_2023),
        ("d", MAY_2021),
    ] {
        model.put_item(&item(link, capture));
    }
    assert_eq!(model.years(), [2021, 2023]);
    assert_eq!(model.months_of(2023), [3, 7]);
    assert_eq!(model.months_of(2021), [5]);
    // No names yet: the months exist but list nothing.
    assert!(model.entries((2023, 3)).is_empty());
    assert!(!model.complete((2023, 3)));
    assert_eq!(model.unnamed((2023, 3)).len(), 2);
    // Newest first.
    assert_eq!(model.unnamed((2023, 3))[0], LinkId::new("b"));
    model.set_meta(&LinkId::new("a"), meta("a.jpg"));
    assert_eq!(model.entries((2023, 3)).len(), 1);
    assert!(model.opened((2023, 3)) && !model.opened((2023, 7)));
    assert_eq!(model.unfinished(), [(2023, 3)]);
    // A name stays when the timeline is read again.
    model.put_item(&item("a", MARCH_2023));
    assert_eq!(model.shown_name(&LinkId::new("a")), Some("a.jpg"));
    // A capture time that moves a photo moves it to its new month.
    model.put_item(&item("a", JULY_2023));
    assert_eq!(model.month_of(&LinkId::new("a")), Some((2023, 7)));
    assert!(model.entries((2023, 3)).is_empty());
    // A claimed time no four-digit year holds goes to the epoch's month.
    model.put_item(&item("far", 400_000_000_000));
    model.put_item(&item("before", -100_000_000_000));
    assert_eq!(model.month_of(&LinkId::new("far")), Some((1970, 1)));
    assert_eq!(model.month_of(&LinkId::new("before")), Some((1970, 1)));
}

#[test]
fn names_that_clash_or_cannot_be_file_names() {
    let mut model = Model::new(utc_month);
    for link in ["x1", "x2", "x3", "x4"] {
        model.put_item(&item(link, MARCH_2023));
    }
    model.set_meta(&LinkId::new("x1"), meta("IMG.jpg"));
    model.set_meta(&LinkId::new("x2"), meta("IMG.jpg"));
    model.set_meta(&LinkId::new("x3"), meta("a/b"));
    model.set_meta(&LinkId::new("x4"), meta(".."));
    let names: Vec<String> = model
        .entries((2023, 3))
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(names, ["IMG (2).jpg", "IMG.jpg"]);
    assert_eq!(model.shown_name(&LinkId::new("x1")), Some("IMG.jpg"));
    assert!(
        model.complete((2023, 3)),
        "names left out still count as known"
    );
    assert_eq!(unique_name("x", |n| n == "x"), "x (2)");
    assert_eq!(unique_name(".rc", |n| n == ".rc"), ".rc (2)");
    let long = "é".repeat(127) + ".jpg";
    let shown = unique_name(&long, |n| n == long);
    assert!(shown.len() <= 255 && shown.ends_with(" (2).jpg"), "{shown}");
    // An extension that leaves no room for the number is cut too (finding
    // 7): 255 bytes, most of them extension.
    let long_ext = format!("a.{}", "x".repeat(253));
    let shown = unique_name(&long_ext, |n| n == long_ext);
    assert!(shown.len() <= 255, "{} bytes", shown.len());
    assert!(shown.starts_with(" (2).xxx"), "{shown}");
    let wide_ext = format!("a.{}", "é".repeat(126));
    let shown = unique_name(&wide_ext, |n| n == wide_ext);
    assert!(shown.len() <= 255 && shown.starts_with(" (2).é"), "{shown}");
}

#[test]
fn a_live_photos_video_sits_beside_it_and_goes_with_it() {
    let mut model = Model::new(utc_month);
    let mut live = item("still", MARCH_2023);
    live.tags = vec![tag::LIVE_PHOTO];
    // The video's own capture time is in another month: it follows the
    // still.
    live.related = vec![RelatedPhoto {
        link: LinkId::new("video"),
        capture_time: JULY_2023,
    }];
    model.put_item(&live);
    model.put_item(&item("other", MARCH_2023));
    assert!(
        model
            .photo(&LinkId::new("still"))
            .is_some_and(Photo::is_live)
    );
    assert_eq!(model.month_of(&LinkId::new("video")), Some((2023, 3)));
    assert_eq!(model.months_of(2023), [3]);
    model.set_meta(&LinkId::new("still"), meta("IMG_1.HEIC"));
    model.set_meta(&LinkId::new("video"), meta("IMG_1.MOV"));
    model.set_meta(&LinkId::new("other"), meta("IMG_2.HEIC"));
    let names: Vec<String> = model
        .entries((2023, 3))
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(names, ["IMG_1.HEIC", "IMG_1.MOV", "IMG_2.HEIC"]);
    // The video goes with its still.
    let removed = model.remove(&LinkId::new("still"));
    assert_eq!(removed.links.len(), 2);
    assert_eq!(removed.shown.len(), 2);
    assert!(model.photo(&LinkId::new("video")).is_none());
    assert_eq!(model.entries((2023, 3)).len(), 1);
    // The last photo of a month takes the month and the year with it.
    model.remove(&LinkId::new("other"));
    assert!(model.years().is_empty());
}

// ---------------------------------------------------------------------------
// The tree over the fake

struct Setup {
    fake: FakeBackend,
    tree: Arc<PhotosTree>,
    _invalidations: mpsc::UnboundedReceiver<Invalidation>,
}

async fn add(fake: &FakeBackend, name: &str, capture: i64) -> LinkId {
    add_tagged(fake, name, capture, Vec::new(), None).await
}

async fn add_tagged(
    fake: &FakeBackend,
    name: &str,
    capture: i64,
    tags: Vec<u8>,
    main: Option<LinkId>,
) -> LinkId {
    let content = Bytes::from(format!("content of {name}").into_bytes());
    fake.add_photo(name, UploadSource::Bytes(content), capture, tags, main)
        .await
        .expect("add photo")
}

async fn open_tree(fake: &FakeBackend, dir: &std::path::Path) -> Setup {
    open_tree_with(fake, dir, Arc::new(fake.clone()), None, Config::default()).await
}

/// [`open_tree`] with its Photos API, thumbnails and config.
async fn open_tree_with(
    fake: &FakeBackend,
    dir: &std::path::Path,
    photos: Arc<dyn Photos>,
    thumbnails: Option<ThumbnailOptions>,
    config: Config,
) -> Setup {
    let tree_backend = fake.photos_tree().expect("photos tree");
    let volume = fake
        .photos_volume_now()
        .expect("the fake has a Photos volume");
    let options = PhotosOptions {
        volume: volume.volume,
        root: volume.root,
        state_db: dir.join("state.db"),
        cache: CacheOptions {
            dir: dir.join("blocks"),
            master_key: Some([7; 32]),
            ram_bytes: 16 << 20,
            disk_bytes: 64 << 20,
            shared: None,
            downloads: None,
        },
        config,
        health: None,
        placer: utc_month,
        thumbnails,
        scan: None,
        limits: None,
    };
    let (tree, invalidations) = PhotosTree::open(Arc::new(tree_backend), photos, options)
        .await
        .expect("open");
    tree.gate.set_interval(Duration::ZERO);
    Setup {
        fake: fake.clone(),
        tree,
        _invalidations: invalidations,
    }
}

fn names(tree: &PhotosTree, ino: Ino) -> Vec<String> {
    let (handle, _) = tree.opendir(ino).expect("opendir");
    let entries = tree.dir_snapshot(handle).expect("snapshot");
    tree.releasedir(handle);
    entries
        .iter()
        .map(|entry| entry.name.clone())
        .filter(|name| name != "." && name != "..")
        .collect()
}

fn ino_of(tree: &PhotosTree, parent: Ino, name: &str) -> Ino {
    match tree.lookup(parent, name.as_bytes()).expect("lookup") {
        Lookup::Found(attr) => attr.ino,
        Lookup::Negative => panic!("{name} not found"),
    }
}

/// Runs the names stage until `month` has all its names.
async fn fetch_all(tree: &Arc<PhotosTree>, month: Month) {
    tree.want(month);
    while let Some((_, batch)) = tree.next_batch() {
        tree.fetch_nodes(&batch).await.expect("names");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_stages_list_years_then_months_then_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeBackend::new();
    for (index, capture) in [MARCH_2023, JULY_2023, MAY_2021].into_iter().enumerate() {
        add(&fake, &format!("IMG_{index}.jpg"), capture).await;
    }
    let setup = open_tree(&fake, dir.path()).await;
    let tree = &setup.tree;
    // Before the timeline: the top folder waits for it.
    assert!(matches!(
        tree.opendir(ROOT_INO),
        Err(CoreError::NeedsListing(1))
    ));
    assert!(matches!(
        tree.lookup(ROOT_INO, b"2023"),
        Err(CoreError::NeedsListing(1))
    ));
    tree.read_timeline().await.expect("timeline");
    assert_eq!(names(tree, ROOT_INO), ["2021", "2023"]);
    assert_eq!(
        tree.lookup(ROOT_INO, b"1999").expect("lookup"),
        Lookup::Negative
    );
    assert_eq!(
        tree.lookup(ROOT_INO, b"notes").expect("lookup"),
        Lookup::Negative
    );
    let year = ino_of(tree, ROOT_INO, "2023");
    assert_eq!(names(tree, year), ["03", "07"]);
    let month = ino_of(tree, year, "03");
    let attr = tree.getattr(month).expect("attr");
    assert_eq!((attr.kind, attr.perm), (NodeKind::Folder, 0o555));
    // A month without names waits for its first batch, and asks for it.
    assert!(matches!(tree.opendir(month), Err(CoreError::NeedsListing(m)) if m == month));
    assert_eq!(
        tree.next_batch().map(|(m, b)| (m, b.len())),
        Some(((2023, 3), 1))
    );
    // A hidden name is answered at once.
    assert_eq!(
        tree.lookup(month, b".hidden").expect("lookup"),
        Lookup::Negative
    );
    fetch_all(tree, (2023, 3)).await;
    assert_eq!(names(tree, month), ["IMG_0.jpg"]);
    // Read with the block cache, as the engine reads.
    let photo = ino_of(tree, month, "IMG_0.jpg");
    let attr = tree.getattr(photo).expect("attr");
    assert_eq!(
        (attr.kind, attr.perm, attr.size),
        (NodeKind::File, 0o444, 20)
    );
    assert_eq!(attr.mtime, time_of(MARCH_2023));
    let (handle, _) = tree.open(photo).expect("open");
    assert!(tree.read_local(handle, 0, 100).expect("local").is_none());
    let data = tree.read(handle, 0, 100).await.expect("read");
    assert_eq!(&data[..], b"content of IMG_0.jpg");
    let (data, _) = tree
        .read_local(handle, 0, 100)
        .expect("local")
        .expect("cached");
    assert_eq!(&data[..], b"content of IMG_0.jpg");
    tree.release(handle);
    assert!(tree.cache_usage().0 > 0, "the block went to the disk cache");
    // The other month knows nothing yet.
    let july = ino_of(tree, year, "07");
    assert!(matches!(
        tree.lookup(july, b"IMG_1.jpg"),
        Err(CoreError::NeedsListing(_))
    ));
    let _ = setup.fake;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_month_whose_names_arrive_in_stages_is_listed_anew() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeBackend::new();
    let count = NAME_BATCH * 2 + 5;
    for index in 0..count {
        add(
            &fake,
            &format!("P{index:03}.jpg"),
            MARCH_2023 + index as i64,
        )
        .await;
    }
    let setup = open_tree(&fake, dir.path()).await;
    let tree = &setup.tree;
    tree.read_timeline().await.expect("timeline");
    let month = ino_of(tree, ino_of(tree, ROOT_INO, "2023"), "03");
    assert!(tree.opendir(month).is_err());
    let (_, batch) = tree.next_batch().expect("a batch");
    tree.fetch_nodes(&batch).await.expect("first batch");
    // The first open sees the first batch.
    let (first, keep) = tree.opendir(month).expect("opendir");
    assert!(!keep);
    assert_eq!(
        tree.dir_snapshot(first).expect("snapshot").len(),
        NAME_BATCH + 2
    );
    tree.releasedir(first);
    // Unchanged: the kernel keeps its pages.
    let (again, keep) = tree.opendir(month).expect("opendir");
    assert!(keep, "same entries, same pages");
    tree.releasedir(again);
    // More names: the pages go (D072), so the listing is not the first one.
    let (_, batch) = tree.next_batch().expect("a batch");
    tree.fetch_nodes(&batch).await.expect("second batch");
    let (second, keep) = tree.opendir(month).expect("opendir");
    assert!(!keep, "new entries: the kernel lists the month again");
    assert_eq!(
        tree.dir_snapshot(second).expect("snapshot").len(),
        2 * NAME_BATCH + 2
    );
    tree.releasedir(second);
    // A lookup of a name still to come waits for the names.
    assert!(matches!(
        tree.lookup(month, b"P000.jpg"),
        Err(CoreError::NeedsListing(_))
    ));
    let waiter = {
        let tree = Arc::clone(tree);
        tokio::spawn(async move { tree.resolve(&CoreError::NeedsListing(month)).await })
    };
    fetch_all(tree, (2023, 3)).await;
    tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("resolve woke up")
        .expect("join")
        .expect("resolved");
    assert_eq!(names(tree, month).len(), count);
    assert!(matches!(
        tree.lookup(month, b"P000.jpg"),
        Ok(Lookup::Found(_))
    ));
    assert_eq!(
        tree.lookup(month, b"nope.jpg").expect("lookup"),
        Lookup::Negative
    );
    // Two handles open at once: neither keeps the pages.
    let (a, _) = tree.opendir(month).expect("opendir");
    let (b, keep) = tree.opendir(month).expect("opendir");
    assert!(!keep);
    tree.releasedir(a);
    tree.releasedir(b);
}

#[tokio::test(flavor = "multi_thread")]
async fn events_bring_new_photos_and_take_deleted_ones() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeBackend::new();
    let old = add(&fake, "old.jpg", MARCH_2023).await;
    add(&fake, "kept.jpg", MARCH_2023 + DAY).await;
    let setup = open_tree(&fake, dir.path()).await;
    let tree = &setup.tree;
    tree.read_timeline().await.expect("timeline");
    fetch_all(tree, (2023, 3)).await;
    // The events since the cursor: the photos the read found already.
    tree.poll_events().await.expect("events");
    let month = ino_of(tree, ino_of(tree, ROOT_INO, "2023"), "03");
    assert_eq!(names(tree, month), ["kept.jpg", "old.jpg"]);

    // A new photo, in a month never opened: named at once from its event.
    add(&fake, "new.jpg", JULY_2023).await;
    let photos = fake.photos_tree().expect("tree");
    photos.trash(&[old]).await.expect("trash");
    tree.poll_events().await.expect("events");
    assert_eq!(names(tree, month), ["kept.jpg"]);
    let july = ino_of(tree, ino_of(tree, ROOT_INO, "2023"), "07");
    assert_eq!(names(tree, july), ["new.jpg"]);
    // A file in the Photos root that the timeline does not list is left
    // out, and not looked for again.
    let stray = photos
        .upload_new(
            &fake.photos_volume_now().expect("volume").root,
            "draft.jpg",
            UploadSource::Bytes(Bytes::from_static(b"x")),
            pdmount_backend::FileMeta {
                mtime: SystemTime::now(),
                mode: None,
                media_type: "image/jpeg".into(),
                sha1: None,
            },
        )
        .await
        .expect("upload")
        .0;
    tree.poll_events().await.expect("events");
    assert!(tree.strangers.lock().contains(&stray));
    // A remount reads nothing again: names, timeline and cursor are stored.
    let status = tree.status();
    assert_eq!((status.photos, status.named), (2, 2));
    tree.shutdown().await.expect("shutdown");
    drop(setup);
    let setup = open_tree(&fake, dir.path()).await;
    let tree = &setup.tree;
    assert!(tree.status().timeline_complete);
    fake.inject_failure(
        pdmount_backend::fake::FakeOp::Timeline,
        pdmount_backend::BackendError::Other("the timeline is not read again".into()),
    );
    let month = ino_of(tree, ino_of(tree, ROOT_INO, "2023"), "03");
    assert_eq!(names(tree, month), ["kept.jpg"]);
    assert!(tree.next_batch().is_none(), "no names to fetch");
    tree.poll_events()
        .await
        .expect("events from the stored cursor");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refresh_reads_the_timeline_again_and_keeps_the_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeBackend::new();
    add(&fake, "a.jpg", MARCH_2023).await;
    let gone = add(&fake, "b.jpg", MARCH_2023).await;
    let setup = open_tree(&fake, dir.path()).await;
    let tree = &setup.tree;
    tree.read_timeline().await.expect("timeline");
    fetch_all(tree, (2023, 3)).await;
    tree.poll_events().await.expect("events");
    // Gone without an event the tree sees: the refresh's read drops it.
    fake.photos_tree()
        .expect("tree")
        .trash(&[gone])
        .await
        .expect("trash");
    fake.request_volume_refresh(pdmount_backend::fake::PHOTOS_VOLUME);
    tree.poll_events().await.expect("refresh");
    let month = ino_of(tree, ino_of(tree, ROOT_INO, "2023"), "03");
    assert_eq!(names(tree, month), ["a.jpg"]);
    assert!(tree.next_batch().is_none(), "the names were kept");
}

#[tokio::test(flavor = "multi_thread")]
async fn live_photos_from_the_fake() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeBackend::new();
    let still = add_tagged(&fake, "IMG_1.HEIC", MARCH_2023, vec![tag::LIVE_PHOTO], None).await;
    add_tagged(
        &fake,
        "IMG_1.MOV",
        MARCH_2023 + 1,
        vec![tag::VIDEO],
        Some(still.clone()),
    )
    .await;
    let setup = open_tree(&fake, dir.path()).await;
    let tree = &setup.tree;
    tree.read_timeline().await.expect("timeline");
    fetch_all(tree, (2023, 3)).await;
    let month = ino_of(tree, ino_of(tree, ROOT_INO, "2023"), "03");
    assert_eq!(names(tree, month), ["IMG_1.HEIC", "IMG_1.MOV"]);
    let video = ino_of(tree, month, "IMG_1.MOV");
    let (handle, _) = tree.open(video).expect("open");
    assert_eq!(
        &tree.read(handle, 0, 64).await.expect("read")[..],
        b"content of IMG_1.MOV"
    );
    tree.release(handle);
    // Trashing the still takes the video out too.
    tree.poll_events().await.expect("events");
    fake.photos_tree()
        .expect("tree")
        .trash(&[still])
        .await
        .expect("trash");
    tree.poll_events().await.expect("events");
    assert!(tree.lookup(ROOT_INO, b"2023").expect("lookup") == Lookup::Negative);
}

// ---------------------------------------------------------------------------
// Thumbnails (M5.13b)

/// The fake's Photos API, counting the thumbnail requests.
#[derive(Clone)]
struct Counting {
    fake: FakeBackend,
    references: Arc<AtomicUsize>,
    fetches: Arc<AtomicUsize>,
}

impl Counting {
    fn new(fake: &FakeBackend) -> Self {
        Self {
            fake: fake.clone(),
            references: Arc::default(),
            fetches: Arc::default(),
        }
    }

    /// Thumbnail references asked for, and batches fetched.
    fn counts(&self) -> (usize, usize) {
        (
            self.references.load(Ordering::SeqCst),
            self.fetches.load(Ordering::SeqCst),
        )
    }
}

#[async_trait::async_trait]
impl Photos for Counting {
    async fn photos_volume(&self) -> pdmount_backend::Result<Option<PhotosVolume>> {
        self.fake.photos_volume().await
    }

    async fn timeline_page(
        &self,
        volume: &str,
        after: Option<&LinkId>,
    ) -> pdmount_backend::Result<Vec<TimelineItem>> {
        self.fake.timeline_page(volume, after).await
    }

    async fn thumbnail_ref(
        &self,
        volume: &str,
        link: &LinkId,
        revision: &RevisionId,
    ) -> pdmount_backend::Result<Option<ThumbnailRef>> {
        self.references.fetch_add(1, Ordering::SeqCst);
        self.fake.thumbnail_ref(volume, link, revision).await
    }

    async fn fetch_thumbnails(
        &self,
        volume: &str,
        thumbnails: &[ThumbnailRef],
    ) -> pdmount_backend::Result<Vec<pdmount_backend::Result<Bytes>>> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        self.fake.fetch_thumbnails(volume, thumbnails).await
    }
}

const MOUNT: &str = "/mnt/Photos";

fn thumbnails_in(dir: &std::path::Path) -> ThumbnailOptions {
    ThumbnailOptions {
        dir: dir.join("thumbnails"),
        mountpoint: MOUNT.into(),
    }
}

/// Runs the thumbnails' task by hand until nothing is queued.
async fn run_thumbnails(tree: &Arc<PhotosTree>) {
    let thumbnailer = Arc::clone(tree.thumbnailer().expect("thumbnails on"));
    while let Some((_, batch)) = thumbnailer.next_batch() {
        tree.thumbnail_batch(&thumbnailer, batch)
            .await
            .expect("thumbnails");
    }
}

/// The thumbnails in `<dir>/thumbnails/<size>`: their file names.
fn thumbnail_files(dir: &std::path::Path, size: &str) -> Vec<String> {
    let mut names: Vec<String> = match std::fs::read_dir(dir.join("thumbnails").join(size)) {
        Ok(entries) => entries
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .into_string()
                    .expect("utf8")
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

/// The size and the URI and MTime chunks of a thumbnail of `name` (a path
/// below the mount).
fn thumbnail_of(dir: &std::path::Path, size: &str, name: &str) -> (u32, u32, String, String) {
    let uri = thumbnails::file_uri(&std::path::Path::new(MOUNT).join(name));
    let path = dir
        .join("thumbnails")
        .join(size)
        .join(thumbnails::cache_name(&uri));
    let file = std::fs::File::open(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
    let reader = png::Decoder::new(std::io::BufReader::new(file))
        .read_info()
        .expect("a PNG");
    let info = reader.info();
    let text = |key: &str| {
        info.uncompressed_latin1_text
            .iter()
            .find(|chunk| chunk.keyword == key)
            .map(|chunk| chunk.text.clone())
            .unwrap_or_default()
    };
    assert_eq!(text("Thumb::URI"), uri);
    (
        info.width,
        info.height,
        text("Thumb::URI"),
        text("Thumb::MTime"),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn an_opened_months_thumbnails_are_written_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeBackend::new();
    add(&fake, "IMG_1.jpg", MARCH_2023).await;
    let small = add(&fake, "IMG_2.jpg", MARCH_2023 + DAY).await;
    let small_thumbnail = FakeThumbnail {
        width: 100,
        height: 60,
        ..FakeThumbnail::DEFAULT
    };
    fake.set_photo_thumbnail(&small, Some(small_thumbnail))
        .expect("thumbnail");
    let without = add(&fake, "IMG_3.jpg", MARCH_2023 + 2 * DAY).await;
    fake.set_photo_thumbnail(&without, None).expect("none");
    add(&fake, "IMG_4.jpg", JULY_2023).await;
    let counting = Counting::new(&fake);
    let setup = open_tree_with(
        &fake,
        dir.path(),
        Arc::new(counting.clone()),
        Some(thumbnails_in(dir.path())),
        Config::default(),
    )
    .await;
    let tree = &setup.tree;
    tree.read_timeline().await.expect("timeline");
    fetch_all(tree, (2023, 3)).await;
    let year = ino_of(tree, ROOT_INO, "2023");
    let march = ino_of(tree, year, "03");
    assert_eq!(names(tree, march).len(), 3);
    run_thumbnails(tree).await;
    // A reference per photo, one batch of downloads.
    assert_eq!(counting.counts(), (3, 1));
    for size in ["normal", "large", "x-large"] {
        assert_eq!(thumbnail_files(dir.path(), size).len(), 2, "{size}");
    }
    let mtime = MARCH_2023.to_string();
    for (size, side) in thumbnails::SIZES {
        let (width, height, uri, at) = thumbnail_of(dir.path(), size, "2023/03/IMG_1.jpg");
        assert_eq!((width, height), (side, side * 3 / 4), "{size}");
        assert_eq!(uri, format!("file://{MOUNT}/2023/03/IMG_1.jpg"));
        assert_eq!(at, mtime);
        // Never scaled up.
        let (width, height, _, _) = thumbnail_of(dir.path(), size, "2023/03/IMG_2.jpg");
        assert_eq!((width, height), (100, 60), "{size}");
    }
    // Opened again: nothing is asked for, nothing written.
    let before = std::fs::metadata(
        dir.path()
            .join("thumbnails/normal")
            .join(&thumbnail_files(dir.path(), "normal")[0]),
    )
    .expect("stat")
    .modified()
    .expect("mtime");
    names(tree, march);
    run_thumbnails(tree).await;
    assert_eq!(counting.counts(), (3, 1));
    // July was never opened.
    assert_eq!(thumbnail_files(dir.path(), "normal").len(), 2);
    // A new session finds them on disk: only the photo without one is
    // asked for again.
    tree.shutdown().await.expect("shutdown");
    drop(setup);
    let counting = Counting::new(&fake);
    let setup = open_tree_with(
        &fake,
        dir.path(),
        Arc::new(counting.clone()),
        Some(thumbnails_in(dir.path())),
        Config::default(),
    )
    .await;
    let tree = &setup.tree;
    let march = ino_of(tree, ino_of(tree, ROOT_INO, "2023"), "03");
    names(tree, march);
    run_thumbnails(tree).await;
    assert_eq!(counting.counts(), (1, 0));
    let after = std::fs::metadata(
        dir.path()
            .join("thumbnails/normal")
            .join(&thumbnail_files(dir.path(), "normal")[0]),
    )
    .expect("stat")
    .modified()
    .expect("mtime");
    assert_eq!(before, after, "not written again");
}

#[tokio::test(flavor = "multi_thread")]
async fn tampered_corrupt_or_oversized_thumbnails_are_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeBackend::new();
    add(&fake, "good.jpg", MARCH_2023).await;
    for (name, thumbnail) in [
        (
            "tampered.jpg",
            FakeThumbnail {
                tampered: true,
                ..FakeThumbnail::DEFAULT
            },
        ),
        (
            "not-webp.jpg",
            FakeThumbnail {
                not_webp: true,
                ..FakeThumbnail::DEFAULT
            },
        ),
        (
            "huge.jpg",
            FakeThumbnail {
                width: 2000,
                height: 4,
                ..FakeThumbnail::DEFAULT
            },
        ),
    ] {
        let link = add(&fake, name, MARCH_2023 + DAY).await;
        fake.set_photo_thumbnail(&link, Some(thumbnail))
            .expect("thumbnail");
    }
    let counting = Counting::new(&fake);
    let setup = open_tree_with(
        &fake,
        dir.path(),
        Arc::new(counting.clone()),
        Some(thumbnails_in(dir.path())),
        Config::default(),
    )
    .await;
    let tree = &setup.tree;
    tree.read_timeline().await.expect("timeline");
    fetch_all(tree, (2023, 3)).await;
    let march = ino_of(tree, ino_of(tree, ROOT_INO, "2023"), "03");
    names(tree, march);
    run_thumbnails(tree).await;
    assert_eq!(counting.counts(), (4, 1));
    // Only the good one is written.
    for size in ["normal", "large", "x-large"] {
        assert_eq!(thumbnail_files(dir.path(), size).len(), 1, "{size}");
    }
    thumbnail_of(dir.path(), "normal", "2023/03/good.jpg");
    // Refused ones are not asked for again in this session.
    names(tree, march);
    run_thumbnails(tree).await;
    assert_eq!(counting.counts(), (4, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_config_switch_turns_thumbnails_off() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeBackend::new();
    add(&fake, "IMG_1.jpg", MARCH_2023).await;
    let counting = Counting::new(&fake);
    let mut config = Config::default();
    config.photos.thumbnail_cache = false;
    let setup = open_tree_with(
        &fake,
        dir.path(),
        Arc::new(counting.clone()),
        Some(thumbnails_in(dir.path())),
        config,
    )
    .await;
    let tree = &setup.tree;
    assert!(tree.thumbnailer().is_none());
    assert!(tree.hook.read().is_none());
    tree.read_timeline().await.expect("timeline");
    fetch_all(tree, (2023, 3)).await;
    names(tree, ino_of(tree, ino_of(tree, ROOT_INO, "2023"), "03"));
    assert_eq!(counting.counts(), (0, 0));
    assert!(!dir.path().join("thumbnails").exists());
    // On by default.
    let setup = open_tree_with(
        &fake,
        &dir.path().join("other"),
        Arc::new(counting),
        Some(thumbnails_in(dir.path())),
        Config::default(),
    )
    .await;
    assert!(setup.tree.thumbnailer().is_some());
}

// ---------------------------------------------------------------------------
// Review fixes (D119)

/// The fake's Photos API with a script for its timeline: the calls
/// counted, one of them failing, and the anchor of one trashed just before
/// it is answered (a photo deleted between two pages).
#[derive(Clone, Default)]
struct Scripted {
    fake: FakeBackend,
    calls: Arc<AtomicUsize>,
    /// The timeline call, counted from 1, that fails.
    fail: Arc<parking_lot::Mutex<Option<usize>>>,
    /// The timeline call before which the link it asks after is trashed.
    trash_anchor: Arc<parking_lot::Mutex<Option<usize>>>,
}

impl Scripted {
    fn new(fake: &FakeBackend) -> Self {
        Self {
            fake: fake.clone(),
            ..Self::default()
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Photos for Scripted {
    async fn photos_volume(&self) -> pdmount_backend::Result<Option<PhotosVolume>> {
        self.fake.photos_volume().await
    }

    async fn timeline_page(
        &self,
        volume: &str,
        after: Option<&LinkId>,
    ) -> pdmount_backend::Result<Vec<TimelineItem>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if *self.fail.lock() == Some(call) {
            return Err(pdmount_backend::BackendError::Transient(
                "scripted failure".into(),
            ));
        }
        if *self.trash_anchor.lock() == Some(call)
            && let Some(anchor) = after
        {
            let photos = self.fake.photos_tree().expect("tree");
            photos
                .trash(std::slice::from_ref(anchor))
                .await
                .expect("trash");
        }
        self.fake.timeline_page(volume, after).await
    }

    async fn thumbnail_ref(
        &self,
        volume: &str,
        link: &LinkId,
        revision: &RevisionId,
    ) -> pdmount_backend::Result<Option<ThumbnailRef>> {
        self.fake.thumbnail_ref(volume, link, revision).await
    }

    async fn fetch_thumbnails(
        &self,
        volume: &str,
        thumbnails: &[ThumbnailRef],
    ) -> pdmount_backend::Result<Vec<pdmount_backend::Result<Bytes>>> {
        self.fake.fetch_thumbnails(volume, thumbnails).await
    }
}

/// A file in the Photos root that the timeline does not list yet: a draft
/// still uploading.
async fn draft(fake: &FakeBackend, name: &str) -> LinkId {
    let photos = fake.photos_tree().expect("tree");
    photos
        .upload_new(
            &fake.photos_volume_now().expect("volume").root,
            name,
            UploadSource::Bytes(Bytes::from_static(b"x")),
            pdmount_backend::FileMeta {
                mtime: SystemTime::now(),
                mode: None,
                media_type: "image/jpeg".into(),
                sha1: None,
            },
        )
        .await
        .expect("upload")
        .0
}

/// Finding 1: a new photo whose timeline place cannot be read (a page that
/// failed, as one with a tag the SDK refused did) no longer holds the
/// cursor: the events after it apply, and it is found at the next poll.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_search_for_a_new_photo_does_not_hold_the_cursor() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeBackend::new();
    let old = add(&fake, "old.jpg", MARCH_2023).await;
    add(&fake, "kept.jpg", MARCH_2023 + DAY).await;
    let scripted = Scripted::new(&fake);
    let setup = open_tree_with(
        &fake,
        dir.path(),
        Arc::new(scripted.clone()),
        None,
        Config::default(),
    )
    .await;
    let tree = &setup.tree;
    tree.read_timeline().await.expect("timeline");
    fetch_all(tree, (2023, 3)).await;
    tree.poll_events().await.expect("events");

    let new = add(&fake, "new.jpg", JULY_2023).await;
    *scripted.fail.lock() = Some(scripted.calls() + 1);
    tree.poll_events()
        .await
        .expect("the events are applied all the same");
    let photos = fake.photos_tree().expect("tree");
    let latest = photos.latest_event_id().await.expect("latest");
    assert_eq!(*tree.cursor.lock(), Some(latest), "the cursor moved on");
    assert!(tree.pending.lock().contains(&new), "the new photo waits");
    assert_eq!(tree.status().pending, 1);

    // The next poll: a later event applies, and the new photo is found.
    photos.trash(&[old]).await.expect("trash");
    tree.poll_events().await.expect("events");
    let year = ino_of(tree, ROOT_INO, "2023");
    assert_eq!(names(tree, ino_of(tree, year, "03")), ["kept.jpg"]);
    assert_eq!(names(tree, ino_of(tree, year, "07")), ["new.jpg"]);
    assert!(tree.pending.lock().is_empty());

    // What waits is stored: a remount tries it again.
    *scripted.fail.lock() = Some(scripted.calls() + 1);
    let later = add(&fake, "later.jpg", JULY_2023 + DAY).await;
    tree.poll_events().await.expect("events");
    assert!(tree.pending.lock().contains(&later));
    tree.shutdown().await.expect("shutdown");
    drop(setup);
    let setup = open_tree(&fake, dir.path()).await;
    let tree = &setup.tree;
    assert!(tree.pending.lock().contains(&later), "stored");
    tree.poll_events().await.expect("events");
    let july = ino_of(tree, ino_of(tree, ROOT_INO, "2023"), "07");
    assert_eq!(names(tree, july), ["later.jpg", "new.jpg"]);
    assert_eq!(tree.status().pending, 0);
}

/// Finding 1, through the fake: a tag the SDK's enum does not know and a
/// capture time chrono cannot hold are shown like any photo.
#[tokio::test(flavor = "multi_thread")]
async fn photos_with_new_tags_and_far_capture_times_are_shown() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeBackend::new();
    add_tagged(&fake, "far.jpg", i64::MAX, vec![10, 200], None).await;
    add_tagged(
        &fake,
        "IMG.jpg",
        MARCH_2023,
        vec![tag::FAVORITE, 9, 31],
        None,
    )
    .await;
    let setup = open_tree(&fake, dir.path()).await;
    let tree = &setup.tree;
    tree.read_timeline().await.expect("timeline");
    assert_eq!(names(tree, ROOT_INO), ["1970", "2023"]);
    fetch_all(tree, (1970, 1)).await;
    let month = ino_of(tree, ino_of(tree, ROOT_INO, "1970"), "01");
    assert_eq!(names(tree, month), ["far.jpg"]);
}

/// Finding 2: a photo deleted between two pages of a read ends no read
/// early, so nothing after it is taken for gone; and a read that fails
/// goes on from its last page, not from the first.
#[tokio::test(flavor = "multi_thread")]
async fn a_photo_deleted_between_two_pages_ends_no_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeBackend::new();
    let count = TIMELINE_PAGE + 10;
    for index in 0..count {
        add(
            &fake,
            &format!("P{index:04}.jpg"),
            MARCH_2023 + index as i64,
        )
        .await;
    }
    let scripted = Scripted::new(&fake);
    let setup = open_tree_with(
        &fake,
        dir.path(),
        Arc::new(scripted.clone()),
        None,
        Config::default(),
    )
    .await;
    let tree = &setup.tree;
    // The second page fails: nothing goes, and the next try asks for the
    // second page again.
    *scripted.fail.lock() = Some(2);
    assert!(tree.read_timeline().await.is_err());
    assert!(!tree.status().timeline_complete);
    tree.read_timeline().await.expect("read on");
    assert_eq!(scripted.calls(), 3, "page 1, page 2 failed, page 2");
    assert_eq!(tree.status().photos, count);
    tree.poll_events().await.expect("events");

    // A refresh reads the whole timeline again; the last photo of its
    // first page is deleted before the second page is asked for.
    *scripted.trash_anchor.lock() = Some(scripted.calls() + 2);
    fake.request_volume_refresh(pdmount_backend::fake::PHOTOS_VOLUME);
    tree.poll_events().await.expect("refresh");
    let status = tree.status();
    assert!(status.timeline_complete);
    assert_eq!(status.photos, count, "nothing after the deleted photo went");
    // The deleted one goes with its event.
    tree.poll_events().await.expect("events");
    assert_eq!(tree.status().photos, count - 1);
    // A remount keeps them all.
    tree.shutdown().await.expect("shutdown");
    drop(setup);
    let setup = open_tree(&fake, dir.path()).await;
    assert_eq!(setup.tree.status().photos, count - 1);
}

/// Finding 4: a photo whose node cannot be decrypted is kept as it was:
/// with its name when it had one, and without one it is not waited for
/// (its month lists the others), until an event reads it again.
#[tokio::test(flavor = "multi_thread")]
async fn a_photo_that_cannot_be_decrypted_is_kept() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeBackend::new();
    let named = add(&fake, "a.jpg", MARCH_2023).await;
    add(&fake, "b.jpg", MARCH_2023 + DAY).await;
    let unnamed = add(&fake, "c.jpg", JULY_2023).await;
    add(&fake, "d.jpg", JULY_2023 + DAY).await;
    let setup = open_tree(&fake, dir.path()).await;
    let tree = &setup.tree;
    tree.read_timeline().await.expect("timeline");
    fetch_all(tree, (2023, 3)).await;
    tree.poll_events().await.expect("events");
    let year = ino_of(tree, ROOT_INO, "2023");
    let march = ino_of(tree, year, "03");

    // A named photo: an event reads its node, which cannot be decrypted.
    let photos = fake.photos_tree().expect("tree");
    photos
        .rename(&named, None, "a2.jpg", None)
        .await
        .expect("rename");
    fake.set_degraded(&named, true);
    tree.poll_events().await.expect("events");
    assert_eq!(names(tree, march), ["a.jpg", "b.jpg"]);

    // An unnamed photo, when its month is opened.
    fake.set_degraded(&unnamed, true);
    let july = ino_of(tree, year, "07");
    fetch_all(tree, (2023, 7)).await;
    assert_eq!(names(tree, july), ["d.jpg"]);
    assert!(tree.next_batch().is_none(), "not asked for again and again");
    let status = tree.status();
    assert_eq!((status.photos, status.unreadable), (4, 1));

    // Readable again: their next events read them.
    fake.set_degraded(&named, false);
    fake.set_degraded(&unnamed, false);
    photos
        .rename(&unnamed, None, "c2.jpg", None)
        .await
        .expect("rename");
    photos
        .rename(&named, None, "a3.jpg", None)
        .await
        .expect("rename");
    tree.poll_events().await.expect("events");
    assert_eq!(names(tree, march), ["a3.jpg", "b.jpg"]);
    assert_eq!(names(tree, july), ["c2.jpg", "d.jpg"]);
    assert_eq!(tree.status().unreadable, 0);
}

/// Finding 6: a link set aside as a stranger (a draft the timeline does
/// not list yet) is looked at again at its next event, its commit; and a
/// search beyond the timeline's first page runs at most every
/// `DEEP_SEARCH_INTERVAL`, the first page at every poll.
#[tokio::test(flavor = "multi_thread")]
async fn a_stranger_is_looked_at_again_at_its_next_event() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeBackend::new();
    for index in 0..TIMELINE_PAGE + 5 {
        add(
            &fake,
            &format!("P{index:04}.jpg"),
            MARCH_2023 + index as i64,
        )
        .await;
    }
    let scripted = Scripted::new(&fake);
    let setup = open_tree_with(
        &fake,
        dir.path(),
        Arc::new(scripted.clone()),
        None,
        Config::default(),
    )
    .await;
    let tree = &setup.tree;
    tree.read_timeline().await.expect("timeline");
    tree.poll_events().await.expect("events");

    // A draft: the whole timeline (two pages) does not list it.
    let first = draft(&fake, "draft-1.jpg").await;
    let before = scripted.calls();
    tree.poll_events().await.expect("events");
    assert_eq!(
        scripted.calls() - before,
        2,
        "a search of the whole timeline"
    );
    assert!(tree.strangers.lock().contains(&first));

    // Its commit, with an old capture time: looked for again. The search
    // beyond the first page is not due yet, so it waits for that.
    fake.commit_photo(&first, MAY_2021, Vec::new())
        .expect("commit");
    let before = scripted.calls();
    tree.poll_events().await.expect("events");
    assert_eq!(scripted.calls() - before, 1, "the first page only");
    assert!(!tree.strangers.lock().contains(&first));
    assert!(tree.pending.lock().contains(&first));
    // Due: the next poll finds it in the second page.
    *tree.last_deep_search.lock() = Some(Instant::now() - sync::DEEP_SEARCH_INTERVAL);
    tree.poll_events().await.expect("events");
    assert!(tree.pending.lock().is_empty());
    fetch_all(tree, (2021, 5)).await;
    let month = ino_of(tree, ino_of(tree, ROOT_INO, "2021"), "05");
    assert_eq!(names(tree, month), ["draft-1.jpg"]);

    // A draft committed with a new capture time: found on the first page,
    // at once, with no search beyond it.
    let second = draft(&fake, "draft-2.jpg").await;
    tree.poll_events().await.expect("events");
    assert!(tree.pending.lock().contains(&second), "not due: it waits");
    fake.commit_photo(&second, JULY_2023, Vec::new())
        .expect("commit");
    let before = scripted.calls();
    tree.poll_events().await.expect("events");
    assert_eq!(scripted.calls() - before, 1);
    let july = ino_of(tree, ino_of(tree, ROOT_INO, "2023"), "07");
    assert_eq!(names(tree, july), ["draft-2.jpg"]);
}

/// Finding 3: a background task that panics runs again, and `status`
/// counts it.
#[tokio::test(flavor = "multi_thread")]
async fn a_task_that_panics_runs_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeBackend::new();
    add(&fake, "a.jpg", MARCH_2023).await;
    let setup = open_tree(&fake, dir.path()).await;
    let tree = &setup.tree;
    let runs = Arc::new(AtomicUsize::new(0));
    let handle = {
        let runs = Arc::clone(&runs);
        tree.supervised("test", move |_| {
            let runs = Arc::clone(&runs);
            async move {
                if runs.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("a test panic");
                }
            }
        })
    };
    tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("it ends")
        .expect("join");
    assert_eq!(runs.load(Ordering::SeqCst), 2);
    assert_eq!(tree.status().restarts, 1);
}

mod session;
