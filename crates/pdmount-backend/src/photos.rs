//! Photos (M5.13, D080, D081, D113): the user's Photos volume and its
//! timeline.
//!
//! Photos is a volume of its own with its own event stream (M5.9). Its
//! photos are file nodes with the one Photos root as parent, so a tree
//! backend for that volume and root ([`crate::proton::ProtonBackend::photos_tree`],
//! [`crate::fake::FakeBackend::tree`]) reads their metadata, block lists and
//! blocks, and polls the volume's events, as for any tree. What only Photos
//! has is here: where the volume is, and the timeline, which gives each
//! photo's capture time, tags and related photos (Live Photos) without its
//! name: names come from the nodes.
//!
//! Everything here comes from the server; capture times and tags are as the
//! uploading client claimed them.

use crate::{LinkId, Result, RevisionId};
use bytes::Bytes;

/// Photos per timeline page: the API's default `PageSize`, which the
/// official clients use (M5.9).
pub const TIMELINE_PAGE: usize = 500;

/// Thumbnails whose storage URLs one request asks for: the official C#
/// SDK's `MaxThumbnailIdsPerRequest` (M5.13b, D115).
pub const THUMBNAIL_BATCH: usize = 30;

/// The most bytes of one thumbnail, encrypted or decrypted: Proton's
/// 512 px WebP is about 26 KB (M5.9), so anything near this is not one.
pub const MAX_THUMBNAIL_BYTES: usize = 1 << 20;

/// A photo's 512 px thumbnail (Proton's type 1, never the 1920 px preview,
/// D107) as its revision's block listing names it, once the listing's
/// manifest signature, which covers the thumbnails' digests, is verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailRef {
    pub link: LinkId,
    pub revision: RevisionId,
    /// The server's id of the thumbnail.
    pub id: String,
    /// SHA-256 of the encrypted thumbnail, as the signed manifest has it.
    pub digest: Vec<u8>,
    /// Its encrypted size, as listed.
    pub size: u64,
}

/// Tags of a photo (`PhotoTag`, `sdk/node/photo.rs`).
pub mod tag {
    pub const FAVORITE: u8 = 0;
    pub const SCREENSHOT: u8 = 1;
    pub const VIDEO: u8 = 2;
    /// An Apple Live Photo: a still with its short video as a related photo.
    pub const LIVE_PHOTO: u8 = 3;
    /// An Android Motion Photo: the video is usually inside the still; it
    /// may come as a related photo too.
    pub const MOTION_PHOTO: u8 = 4;
}

/// Where the user's photos are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhotosVolume {
    pub volume: String,
    /// The Photos root folder, the parent of every photo.
    pub root: LinkId,
}

/// A photo that belongs to another one: a Live Photo's video, say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelatedPhoto {
    pub link: LinkId,
    /// Seconds since the epoch, as claimed.
    pub capture_time: i64,
}

/// One item of the timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineItem {
    pub link: LinkId,
    /// Seconds since the epoch, as claimed by the uploader.
    pub capture_time: i64,
    /// [`tag`] values.
    pub tags: Vec<u8>,
    pub related: Vec<RelatedPhoto>,
}

/// What a Photos mount needs beyond the tree backend of its volume.
#[async_trait::async_trait]
pub trait Photos: Send + Sync + 'static {
    /// The user's Photos volume and root; `None` when the account has none
    /// (the official clients make it at the first upload).
    async fn photos_volume(&self) -> Result<Option<PhotosVolume>>;

    /// One page of the timeline of `volume`, newest first: the first page
    /// with `after` `None`, then the page after the item `after` (the last
    /// of the page before, `PreviousPageLastLinkID`). A page shorter than
    /// [`TIMELINE_PAGE`] is the last.
    async fn timeline_page(
        &self,
        volume: &str,
        after: Option<&LinkId>,
    ) -> Result<Vec<TimelineItem>>;

    /// The 512 px thumbnail of revision `revision` of photo `link` on
    /// `volume`: from the first page of the revision's block listing (one
    /// request, plus the signer's keys once), its manifest signature
    /// verified as a block read's when that page is the whole listing; a
    /// larger revision's thumbnail is checked by decryption alone (D135).
    /// `None` when the revision has none; `Integrity` when the manifest
    /// does not verify.
    async fn thumbnail_ref(
        &self,
        volume: &str,
        link: &LinkId,
        revision: &RevisionId,
    ) -> Result<Option<ThumbnailRef>>;

    /// Downloads and decrypts `thumbnails` (at most [`THUMBNAIL_BATCH`]):
    /// their storage URLs in one request, then each one, whose SHA-256 must
    /// be its `digest` before it is decrypted. One result per thumbnail, in
    /// order: its decrypted bytes, or why not (`Integrity` for a digest
    /// that differs or more than [`MAX_THUMBNAIL_BYTES`]).
    async fn fetch_thumbnails(
        &self,
        volume: &str,
        thumbnails: &[ThumbnailRef],
    ) -> Result<Vec<Result<Bytes>>>;
}
