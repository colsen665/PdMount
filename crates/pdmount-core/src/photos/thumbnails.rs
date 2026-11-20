//! Proton's thumbnails in the freedesktop thumbnail cache (M5.13b, D081,
//! D107, D115), so a file manager shows a month of photos without opening
//! the originals.
//!
//! When a month is opened, [`Thumbnailer`] (the tree's [`ThumbnailHook`])
//! queues its named photos; a task of the tree takes them in batches of
//! [`THUMBNAIL_BATCH`], the month opened last first:
//!
//! 1. **What is there:** a photo whose thumbnails of every size are there
//!    and valid for it (same `Thumb::URI` and `Thumb::MTime`, and the same
//!    revision when PdMount wrote them) is skipped, and so is one noted in
//!    this session as done or as having no usable thumbnail.
//! 2. **Its reference**, one request per photo and one background turn of
//!    the session's gate each (the pacing of the names, D027, D110; one
//!    turn queued at a time, so it neither starves the other trees'
//!    background work nor is starved by it): the first page of the
//!    revision's block listing, whose manifest signature covers the
//!    thumbnails' digests, verified when the page is the whole listing
//!    (a photo; a video's thumbnail is checked by decryption alone, D135;
//!    [`Photos::thumbnail_ref`]). Only
//!    the 512 px thumbnail, never the 1920 px preview (D107).
//! 3. **The thumbnails** of the batch: their URLs in one request, started
//!    in one more turn, the downloads checked against the signed digests
//!    ([`Photos::fetch_thumbnails`]), all in the session's prefetch download
//!    slot; the turn is not held across the downloads (D120).
//! 4. **The PNGs**, off the runtime: the WebP decoded within bounds
//!    ([`MAX_THUMBNAIL_BYTES`], then every frame's size from its own header
//!    within [`MAX_SIDE`] and the canvas, [`frames`]), scaled down to each size
//!    ([`SIZES`]) and written atomically to
//!    `$XDG_CACHE_HOME/thumbnails/<size>/<md5 of the URI>.png`, with the
//!    URI and modification time the mount reports.
//!
//! Thumbnail bytes are untrusted: a thumbnail too large before decoding or
//! after, one that does not decode, one whose digest differs, or a panic of
//! the decoder, is refused and noted for the session; nothing is written
//! for it.

use super::model::Month;
use super::{PhotoView, PhotosTree, ThumbnailHook};
use crate::cache::Priority;
use crate::errors::{CoreError, Result};
use crate::health::Connectivity;
use crate::metrics::check_not_hot;
use crate::poller::backoff;
use image::{DynamicImage, GenericImageView, RgbImage, RgbaImage};
use pdmount_backend::photos::{MAX_THUMBNAIL_BYTES, THUMBNAIL_BATCH, ThumbnailRef};
use pdmount_backend::{LinkId, RevisionId};
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

pub mod frames;

/// The sizes written: the freedesktop directory and its longest side.
/// `x-large` (spec 0.9) is Proton's 512 px thumbnail as it comes; there is
/// no `xx-large`, which would be scaled up (D107).
pub const SIZES: [(&str, u32); 3] = [("normal", 128), ("large", 256), ("x-large", 512)];

/// The longest side of a thumbnail accepted: twice the 512 px expected.
pub const MAX_SIDE: u32 = 1024;

/// Key of the revision a thumbnail PdMount wrote was made from: a photo
/// replaced by a new revision keeps its capture time, so `Thumb::MTime`
/// alone would keep the old picture.
pub const REVISION_KEY: &str = "X-PdMount::Revision";

/// The largest thumbnail file read to see whether it is valid.
const MAX_PNG_READ: u64 = 8 << 20;
/// The wait before a failed batch is tried again, at the first failure; it
/// grows as the pollers' does.
const THUMBNAILS_RETRY: Duration = Duration::from_secs(5);

/// Where the thumbnails go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailOptions {
    /// `$XDG_CACHE_HOME/thumbnails`.
    pub dir: PathBuf,
    /// The mount's absolute path, as the file manager sees it: the photos'
    /// URIs are below it.
    pub mountpoint: PathBuf,
}

/// The `file://` URI of `path` as GLib's `g_filename_to_uri` makes it,
/// which is what file managers hash: every byte escaped as `%XX`
/// (upper-case) but ASCII letters, digits and `-._~!$&'()*+,:=@/`.
pub fn file_uri(path: &Path) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~!$&'()*+,:=@/".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            uri.push('%');
            uri.push(char::from(HEX[usize::from(byte >> 4)]));
            uri.push(char::from(HEX[usize::from(byte & 0xf)]));
        }
    }
    uri
}

/// The file name of `uri`'s thumbnail: the MD5 of the URI in lower-case
/// hex, `.png`.
pub fn cache_name(uri: &str) -> String {
    use md5::{Digest, Md5};
    let digest = Md5::digest(uri.as_bytes());
    let mut name = String::with_capacity(36);
    for byte in digest {
        name.push_str(&format!("{byte:02x}"));
    }
    name.push_str(".png");
    name
}

/// What a thumbnail says of its original, and must say to be valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Original {
    pub uri: String,
    /// Seconds since the epoch: the mtime the mount reports.
    pub mtime: i64,
    pub size: u64,
    pub revision: String,
}

impl Original {
    pub fn of(mountpoint: &Path, photo: &PhotoView) -> Self {
        Self {
            uri: file_uri(&mountpoint.join(&photo.path)),
            mtime: seconds(photo.mtime),
            size: photo.size,
            revision: photo.revision.as_str().to_owned(),
        }
    }

    /// The text chunks of its thumbnails.
    fn chunks(&self) -> Vec<(&'static str, String)> {
        let mut chunks = vec![
            ("Thumb::URI", self.uri.clone()),
            ("Thumb::MTime", self.mtime.to_string()),
            ("Thumb::Size", self.size.to_string()),
            ("Software", "PdMount".to_owned()),
        ];
        // A server's id: only when it is plain text.
        if self.revision.len() <= 128 && self.revision.bytes().all(|b| b.is_ascii_graphic()) {
            chunks.push((REVISION_KEY, self.revision.clone()));
        }
        chunks
    }
}

fn seconds(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
        Err(before) => -i64::try_from(before.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

/// Whether the PNG at `path` is a valid thumbnail of `original`: the URI
/// and mtime the spec requires, its `Thumb::Size` and PdMount's revision
/// when it has them.
pub fn is_valid(path: &Path, original: &Original) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    if file
        .metadata()
        .map_or(true, |meta| meta.len() > MAX_PNG_READ)
    {
        return false;
    }
    let Ok(reader) = png::Decoder::new(std::io::BufReader::new(file)).read_info() else {
        return false;
    };
    let text = |key: &str| {
        reader
            .info()
            .uncompressed_latin1_text
            .iter()
            .find(|chunk| chunk.keyword == key)
            .map(|chunk| chunk.text.as_str())
    };
    text("Thumb::URI") == Some(original.uri.as_str())
        && text("Thumb::MTime").and_then(|mtime| mtime.trim().parse::<i64>().ok())
            == Some(original.mtime)
        && text("Thumb::Size").is_none_or(|size| size.trim().parse() == Ok(original.size))
        && text(REVISION_KEY).is_none_or(|revision| revision == original.revision)
}

/// Why a thumbnail was not written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Refused(pub String);

/// Decodes Proton's WebP within bounds: at most [`MAX_THUMBNAIL_BYTES`],
/// then every frame's size read from its own header ([`frames::check`]: at
/// most [`MAX_SIDE`] on either side, within the canvas, at most
/// [`frames::MAX_TOTAL_PIXELS`] in all) before the decoder sees the bytes,
/// and the decoder's own idea of the size checked again.
pub fn decode(webp: &[u8]) -> std::result::Result<DynamicImage, Refused> {
    if webp.len() > MAX_THUMBNAIL_BYTES {
        return Err(Refused(format!(
            "{} bytes, more than a thumbnail's {MAX_THUMBNAIL_BYTES}",
            webp.len()
        )));
    }
    let frames = frames::check(webp)?;
    let mut decoder = image_webp::WebPDecoder::new(std::io::Cursor::new(webp))
        .map_err(|error| Refused(format!("not a WebP: {error}")))?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return Err(Refused(format!(
            "{width} x {height} pixels, not a thumbnail of at most {MAX_SIDE}"
        )));
    }
    // The decoder must have read the headers as the check did.
    if (width, height) != frames.canvas || decoder.is_animated() != frames.animated {
        return Err(Refused(format!(
            "the decoder reads {width} x {height} pixels where the headers say {} x {}",
            frames.canvas.0, frames.canvas.1
        )));
    }
    let len = decoder
        .output_buffer_size()
        .filter(|len| *len <= (MAX_SIDE * MAX_SIDE * 4) as usize)
        .ok_or_else(|| Refused("an implausible decoded size".into()))?;
    decoder.set_memory_limit(len.saturating_mul(2));
    let mut pixels = vec![0; len];
    decoder
        .read_image(&mut pixels)
        .map_err(|error| Refused(format!("the WebP does not decode: {error}")))?;
    let image = if decoder.has_alpha() {
        RgbaImage::from_raw(width, height, pixels).map(DynamicImage::ImageRgba8)
    } else {
        RgbImage::from_raw(width, height, pixels).map(DynamicImage::ImageRgb8)
    };
    image.ok_or_else(|| Refused("the decoded pixels do not fit the size".into()))
}

/// `image` scaled down to fit `side` on its longest side; never scaled up.
pub fn scaled(image: &DynamicImage, side: u32) -> DynamicImage {
    let (width, height) = image.dimensions();
    if width.max(height) <= side {
        image.clone()
    } else {
        image.thumbnail(side, side)
    }
}

/// `image` as a PNG with `original`'s text chunks before the pixels.
pub fn encode_png(
    image: &DynamicImage,
    original: &Original,
) -> std::result::Result<Vec<u8>, Refused> {
    let (width, height) = image.dimensions();
    let (color, pixels) = match image {
        DynamicImage::ImageRgba8(rgba) => (png::ColorType::Rgba, rgba.as_raw().clone()),
        other => (png::ColorType::Rgb, other.to_rgb8().into_raw()),
    };
    let failed = |error: png::EncodingError| Refused(format!("cannot write the PNG: {error}"));
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        for (key, value) in original.chunks() {
            encoder
                .add_text_chunk(key.to_owned(), value)
                .map_err(failed)?;
        }
        let mut writer = encoder.write_header().map_err(failed)?;
        writer.write_image_data(&pixels).map_err(failed)?;
        writer.finish().map_err(failed)?;
    }
    Ok(out)
}

/// Writes `data` to `dir/name` atomically: a temporary file of mode 0600
/// in `dir`, then a rename. `dir` (and any parent missing) is made with
/// mode 0700, as the spec asks.
pub fn write_atomic(dir: &Path, name: &str, data: &[u8]) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let temp = dir.join(format!(".{name}.pdmount-{:016x}", rand::random::<u64>()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .and_then(|mut file| file.write_all(data))
        .and_then(|()| std::fs::rename(&temp, dir.join(name)));
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
}

/// What became of one photo's thumbnails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Written,
    Refused(String),
    /// Writing failed locally: tried again at the next open.
    Failed(String),
}

/// The tree's [`ThumbnailHook`] and what it noted in this session.
pub struct Thumbnailer {
    options: ThumbnailOptions,
    /// Months to look at, the one opened last at the end, each with its
    /// photos not looked at yet (the next one last).
    pending: parking_lot::Mutex<Vec<(Month, Vec<PhotoView>)>>,
    wake: Notify,
    /// Photos whose thumbnails are there, as they were for them.
    done: parking_lot::Mutex<HashMap<LinkId, Original>>,
    /// Photos without a usable thumbnail, per revision.
    refused: parking_lot::Mutex<HashMap<LinkId, RevisionId>>,
}

impl ThumbnailHook for Thumbnailer {
    /// Queues the month, before the months queued earlier.
    fn month_opened(&self, month: Month, mut photos: Vec<PhotoView>) {
        photos.reverse();
        {
            let mut pending = self.pending.lock();
            pending.retain(|(queued, _)| *queued != month);
            pending.push((month, photos));
        }
        self.wake.notify_one();
    }
}

impl Thumbnailer {
    pub fn new(options: ThumbnailOptions) -> Self {
        Self {
            options,
            pending: parking_lot::Mutex::default(),
            wake: Notify::new(),
            done: parking_lot::Mutex::default(),
            refused: parking_lot::Mutex::default(),
        }
    }

    pub fn options(&self) -> &ThumbnailOptions {
        &self.options
    }

    /// Up to [`THUMBNAIL_BATCH`] photos of the month queued last that this
    /// session has not settled yet.
    pub(super) fn next_batch(&self) -> Option<(Month, Vec<(PhotoView, Original)>)> {
        let mut pending = self.pending.lock();
        let done = self.done.lock();
        let refused = self.refused.lock();
        while let Some((month, photos)) = pending.last_mut() {
            let month = *month;
            let mut batch = Vec::new();
            while batch.len() < THUMBNAIL_BATCH
                && let Some(photo) = photos.pop()
            {
                let original = Original::of(&self.options.mountpoint, &photo);
                if done.get(&photo.link) == Some(&original)
                    || refused.get(&photo.link) == Some(&photo.revision)
                {
                    continue;
                }
                batch.push((photo, original));
            }
            if photos.is_empty() {
                pending.pop();
            }
            if !batch.is_empty() {
                return Some((month, batch));
            }
        }
        None
    }

    /// Puts photos of a failed batch back, to be tried first.
    fn requeue(&self, month: Month, photos: Vec<PhotoView>) {
        let mut pending = self.pending.lock();
        match pending.iter_mut().find(|(queued, _)| *queued == month) {
            Some((_, queued)) => queued.extend(photos),
            None => pending.push((month, photos)),
        }
    }

    /// The sizes (indices into [`SIZES`]) whose valid thumbnail of
    /// `original` is missing.
    pub fn missing(&self, original: &Original) -> Vec<usize> {
        let name = cache_name(&original.uri);
        SIZES
            .iter()
            .enumerate()
            .filter(|(_, (dir, _))| !is_valid(&self.options.dir.join(dir).join(&name), original))
            .map(|(index, _)| index)
            .collect()
    }

    /// Notes that the photo's thumbnails are there.
    fn settled(&self, link: &LinkId, original: Original) {
        self.done.lock().insert(link.clone(), original);
    }

    fn refuse(&self, link: &LinkId, revision: &RevisionId, why: &str) {
        tracing::warn!(%link, why, "photos: no thumbnail written for this photo");
        self.refused.lock().insert(link.clone(), revision.clone());
    }

    /// Decodes `webp` and writes the `sizes` of `original`'s thumbnail.
    /// Runs off the runtime; a panic of the decoder refuses the photo.
    pub fn write(&self, webp: &[u8], original: &Original, sizes: &[usize]) -> Outcome {
        let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let image = decode(webp)?;
            sizes
                .iter()
                .map(|&index| {
                    let png = encode_png(&scaled(&image, SIZES[index].1), original)?;
                    Ok((index, png))
                })
                .collect::<std::result::Result<Vec<_>, Refused>>()
        }));
        let pngs = match rendered {
            Ok(Ok(pngs)) => pngs,
            Ok(Err(Refused(why))) => return Outcome::Refused(why),
            Err(_) => return Outcome::Refused("the decoder failed".into()),
        };
        let name = cache_name(&original.uri);
        for (index, png) in pngs {
            let dir = self.options.dir.join(SIZES[index].0);
            if let Err(error) = write_atomic(&dir, &name, &png) {
                return Outcome::Failed(format!("{}: {error}", dir.display()));
            }
        }
        Outcome::Written
    }
}

/// A photo whose thumbnail is to be fetched: its reference, what its
/// thumbnail must say, and the sizes missing.
struct Wanted {
    reference: ThumbnailRef,
    original: Original,
    sizes: Vec<usize>,
}

impl PhotosTree {
    /// The thumbnails of the months opened, a batch at a time, until the
    /// tree stops; a failed batch is tried again with the pollers'
    /// back-off.
    pub(super) async fn run_thumbnails(self: Arc<Self>, thumbnailer: Arc<Thumbnailer>) {
        let mut failures = 0;
        loop {
            let Some((month, batch)) = thumbnailer.next_batch() else {
                thumbnailer.wake.notified().await;
                continue;
            };
            let photos: Vec<PhotoView> = batch.iter().map(|(photo, _)| photo.clone()).collect();
            match self.thumbnail_batch(&thumbnailer, batch).await {
                Ok(()) => failures = 0,
                Err(error) => {
                    thumbnailer.requeue(month, photos);
                    if self.health.state() != Connectivity::Online {
                        failures = 0;
                        continue;
                    }
                    let delay = backoff(THUMBNAILS_RETRY, failures, &error);
                    failures += 1;
                    tracing::warn!(%error, ?delay, "photos: fetching thumbnails failed");
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// One batch: what is missing on disk, then each photo's reference (a
    /// background turn each), the thumbnails (one more), and the PNGs.
    pub(super) async fn thumbnail_batch(
        &self,
        thumbnailer: &Arc<Thumbnailer>,
        batch: Vec<(PhotoView, Original)>,
    ) -> Result<()> {
        let looking = Arc::clone(thumbnailer);
        let missing = tokio::task::spawn_blocking(move || {
            batch
                .into_iter()
                .filter_map(|(photo, original)| {
                    let sizes = looking.missing(&original);
                    if sizes.is_empty() {
                        looking.settled(&photo.link, original);
                        None
                    } else {
                        Some((photo, original, sizes))
                    }
                })
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|error| CoreError::Internal(format!("thumbnail check: {error}")))?;
        if missing.is_empty() {
            return Ok(());
        }
        self.health.wait_online().await;
        let mut wanted = Vec::new();
        for (photo, original, sizes) in missing {
            let found = {
                let _turn = self.gate.background().await;
                check_not_hot(&self.metrics, "thumbnail_ref");
                self.photos
                    .thumbnail_ref(&self.volume, &photo.link, &photo.revision)
                    .await
            };
            match found {
                Ok(Some(reference)) => wanted.push(Wanted {
                    reference,
                    original,
                    sizes,
                }),
                Ok(None) => thumbnailer.refuse(&photo.link, &photo.revision, "it has none"),
                Err(error) if error.is_permanent() => {
                    thumbnailer.refuse(&photo.link, &photo.revision, &error.to_string());
                }
                Err(error) => return Err(error.into()),
            }
        }
        if wanted.is_empty() {
            return Ok(());
        }
        let references: Vec<ThumbnailRef> = wanted.iter().map(|w| w.reference.clone()).collect();
        let fetched = {
            // The batch's downloads hold the session's prefetch download
            // slot (D033); the turn paces the start of their URL request
            // and is not held across the downloads (D120).
            let _slot = self.cache.download_slot(Priority::Prefetch).await?;
            drop(self.gate.background().await);
            check_not_hot(&self.metrics, "fetch_thumbnails");
            self.photos
                .fetch_thumbnails(&self.volume, &references)
                .await?
        };
        let mut work = Vec::new();
        for (wanted, result) in wanted.into_iter().zip(fetched) {
            let reference = &wanted.reference;
            match result {
                Ok(bytes) => work.push((wanted, bytes)),
                Err(error) if error.is_permanent() => {
                    thumbnailer.refuse(&reference.link, &reference.revision, &error.to_string());
                }
                // Tried again at the month's next open.
                Err(error) => {
                    tracing::debug!(link = %reference.link, %error, "photos: a thumbnail's download failed");
                }
            }
        }
        let writing = Arc::clone(thumbnailer);
        let (written, failed) = tokio::task::spawn_blocking(move || {
            let (mut written, mut failed) = (0, None);
            for (wanted, bytes) in work {
                let Wanted {
                    reference,
                    original,
                    sizes,
                } = wanted;
                match writing.write(&bytes, &original, &sizes) {
                    Outcome::Written => {
                        written += 1;
                        writing.settled(&reference.link, original);
                    }
                    Outcome::Refused(why) => {
                        writing.refuse(&reference.link, &reference.revision, &why)
                    }
                    Outcome::Failed(why) => failed = Some(why),
                }
            }
            (written, failed)
        })
        .await
        .map_err(|error| CoreError::Internal(format!("thumbnail writes: {error}")))?;
        if let Some(why) = failed {
            tracing::warn!(why, "photos: cannot write to the thumbnail cache");
        }
        if written > 0 {
            tracing::info!(written, "photos: thumbnails written");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn webp(width: u32, height: u32, alpha: bool) -> Vec<u8> {
        let channels = if alpha { 4 } else { 3 };
        let pixels: Vec<u8> = (0..width * height * channels)
            .map(|i| (i % 251) as u8)
            .collect();
        let color = if alpha {
            image_webp::ColorType::Rgba8
        } else {
            image_webp::ColorType::Rgb8
        };
        let mut out = Vec::new();
        image_webp::WebPEncoder::new(&mut out)
            .encode(&pixels, width, height, color)
            .expect("encode");
        out
    }

    fn original() -> Original {
        Original {
            uri: "file:///mnt/Photos/2023/03/IMG_0001.jpg".into(),
            mtime: 1_678_881_600,
            size: 3000,
            revision: "rev-1".into(),
        }
    }

    /// The size and text chunks of a PNG.
    fn read_png(png: &[u8]) -> (u32, u32, HashMap<String, String>) {
        let reader = png::Decoder::new(std::io::Cursor::new(png))
            .read_info()
            .expect("a PNG");
        let info = reader.info();
        let text = info
            .uncompressed_latin1_text
            .iter()
            .map(|chunk| (chunk.keyword.clone(), chunk.text.clone()))
            .collect();
        (info.width, info.height, text)
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
    }

    #[test]
    fn uris_and_file_names_as_the_spec_and_glib_make_them() {
        // The example of the freedesktop thumbnail spec.
        let uri = file_uri(Path::new("/home/jens/photos/me.png")); // betterleaks:allow
        assert_eq!(uri, "file:///home/jens/photos/me.png"); // betterleaks:allow
        assert_eq!(cache_name(&uri), "c6ee772d9e49320e97ec29a7eb5b1697.png");
        // What GLib's g_filename_to_uri gives for the same path (checked
        // with GLib 2.88): reserved and non-ASCII bytes escaped in upper
        // case, the sub-delimiters, `:` and `@` kept.
        let uri = file_uri(Path::new("/tmp/a b/é#%;?[]{}|^`\"<>!$&'()*+,-.:=@_~"));
        assert_eq!(
            uri,
            "file:///tmp/a%20b/%C3%A9%23%25%3B%3F%5B%5D%7B%7D%7C%5E%60%22%3C%3E!$&'()*+,-.:=@_~"
        );
        assert_eq!(cache_name(&uri), "182a189a72e0c1e844b79edaaec17d43.png");
        let photo = PhotoView {
            link: LinkId::new("l"),
            revision: RevisionId::new("r"),
            path: "2023/03/IMG 1.jpg".into(),
            size: 7,
            mtime: UNIX_EPOCH + Duration::from_secs(1_678_881_600),
        };
        let original = Original::of(Path::new("/mnt/Photos"), &photo);
        assert_eq!(original.uri, "file:///mnt/Photos/2023/03/IMG%201.jpg");
        assert_eq!((original.mtime, original.size), (1_678_881_600, 7));
        let before = PhotoView {
            mtime: UNIX_EPOCH - Duration::from_secs(5),
            ..photo
        };
        assert_eq!(Original::of(Path::new("/m"), &before).mtime, -5);
    }

    #[test]
    fn a_png_with_the_uri_and_mtime_text_chunks() {
        let image = decode(&webp(512, 384, false)).expect("decode");
        let png = encode_png(&image, &original()).expect("png");
        let (width, height, text) = read_png(&png);
        assert_eq!((width, height), (512, 384));
        assert_eq!(text["Thumb::URI"], original().uri);
        assert_eq!(text["Thumb::MTime"], "1678881600");
        assert_eq!(text["Thumb::Size"], "3000");
        assert_eq!(text[REVISION_KEY], "rev-1");
        // Alpha stays.
        let image = decode(&webp(20, 10, true)).expect("decode");
        let png = encode_png(&image, &original()).expect("png");
        let reader = png::Decoder::new(std::io::Cursor::new(&png))
            .read_info()
            .expect("png");
        assert_eq!(reader.info().color_type, png::ColorType::Rgba);
        // A revision id that is no plain text is left out.
        let odd = Original {
            revision: "a b\u{e9}".into(),
            ..original()
        };
        let (_, _, text) = read_png(&encode_png(&image, &odd).expect("png"));
        assert!(!text.contains_key(REVISION_KEY));
    }

    #[test]
    fn scaled_down_to_each_size_and_never_up() {
        let image = decode(&webp(512, 384, false)).expect("decode");
        let sides: Vec<(u32, u32)> = SIZES
            .iter()
            .map(|(_, side)| scaled(&image, *side).dimensions())
            .collect();
        assert_eq!(sides, [(128, 96), (256, 192), (512, 384)]);
        let portrait = decode(&webp(384, 512, false)).expect("decode");
        assert_eq!(scaled(&portrait, 128).dimensions(), (96, 128));
        let small = decode(&webp(100, 60, false)).expect("decode");
        assert_eq!(scaled(&small, 256).dimensions(), (100, 60));
        assert_eq!(scaled(&small, 128).dimensions(), (100, 60));
    }

    #[test]
    fn corrupt_or_oversized_thumbnails_are_refused() {
        assert!(decode(b"not a WebP at all").is_err());
        let mut truncated = webp(64, 64, false);
        truncated.truncate(truncated.len() / 2);
        assert!(decode(&truncated).is_err());
        let wide = webp(MAX_SIDE + 1, 2, false);
        let refused = decode(&wide).expect_err("too wide");
        assert!(refused.0.contains("pixels"), "{refused}");
        assert!(decode(&webp(MAX_SIDE, 2, false)).is_ok());
        let mut huge = webp(8, 8, false);
        huge.resize(MAX_THUMBNAIL_BYTES + 1, 0);
        assert!(decode(&huge).expect_err("too large").0.contains("bytes"));
        // The writer refuses them and writes nothing.
        let dir = tempfile::tempdir().expect("tempdir");
        let thumbnailer = Thumbnailer::new(ThumbnailOptions {
            dir: dir.path().join("thumbnails"),
            mountpoint: "/mnt".into(),
        });
        for bad in [&b"junk"[..], &wide, &huge] {
            assert!(matches!(
                thumbnailer.write(bad, &original(), &[0, 1, 2]),
                Outcome::Refused(_)
            ));
        }
        assert!(!dir.path().join("thumbnails").exists());
    }

    #[test]
    fn written_atomically_private_and_valid_until_the_photo_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("cache/thumbnails");
        let thumbnailer = Thumbnailer::new(ThumbnailOptions {
            dir: root.clone(),
            mountpoint: "/mnt".into(),
        });
        let original = original();
        assert_eq!(thumbnailer.missing(&original), [0, 1, 2]);
        assert_eq!(
            thumbnailer.write(&webp(512, 384, false), &original, &[0, 1, 2]),
            Outcome::Written
        );
        assert!(thumbnailer.missing(&original).is_empty());
        let name = cache_name(&original.uri);
        for (size, side) in SIZES {
            let path = root.join(size).join(&name);
            let (width, height, _) = read_png(&std::fs::read(&path).expect("written"));
            assert_eq!(width.max(height), side);
            assert_eq!(mode(&path), 0o600, "{size}");
            assert_eq!(mode(&root.join(size)), 0o700, "{size}");
        }
        // No temporary file is left.
        let entries = std::fs::read_dir(root.join("normal")).expect("dir").count();
        assert_eq!(entries, 1);
        // Another mtime, size or revision: not valid for it.
        for changed in [
            Original {
                mtime: original.mtime + 1,
                ..original.clone()
            },
            Original {
                size: 1,
                ..original.clone()
            },
            Original {
                revision: "rev-2".into(),
                ..original.clone()
            },
        ] {
            assert_eq!(thumbnailer.missing(&changed), [0, 1, 2], "{changed:?}");
        }
        // One size missing: only it is to be written.
        std::fs::remove_file(root.join("large").join(&name)).expect("rm");
        assert_eq!(thumbnailer.missing(&original), [1]);
        // A file manager's thumbnail, without PdMount's revision, counts.
        let image = decode(&webp(256, 192, false)).expect("decode");
        let theirs = Original {
            revision: "not plain text".into(),
            ..original.clone()
        };
        write_atomic(
            &root.join("large"),
            &name,
            &encode_png(&image, &theirs).expect("png"),
        )
        .expect("write");
        assert!(thumbnailer.missing(&original).is_empty());
        // A file that is no PNG is not valid.
        std::fs::write(root.join("normal").join(&name), b"junk").expect("write");
        assert_eq!(thumbnailer.missing(&original), [0]);
    }
}
