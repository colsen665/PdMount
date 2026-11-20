//! A revision's block list, read by range (DESIGN.md §6.1, D135).
//!
//! A read lists only the page of the block it reads: [`BLOCK_PAGE_SIZE`]
//! blocks from the page's first index, with each block's storage target and
//! listed hash. Block offsets come from the claimed block sizes (fork P4),
//! so no listing is needed to find a block. Every listed block keeps its
//! target until [`BLOCK_TARGET_TTL`] after its listing; an expired one is
//! listed again with its page alone, under that page's lock, so the file's
//! other downloads go on.
//!
//! The listed hashes are authenticated by the revision's manifest signature,
//! which covers every block (fork P17): the first read of a revision lists
//! the other pages in order, one at a time, beside its own download, and
//! its bytes are given only once the manifest verified. The verified hashes
//! stay for as long as the revision is kept, so a page listed again after
//! the TTL needs no new verification: its hashes must equal them. Both the
//! Proton adapter and the fake read through this, so the fake measures it.

use crate::{BackendError, LinkId, Result, RevisionId};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// Blocks per page: the official JS SDK's `BLOCKS_PAGE_SIZE`
/// (`cs/client/js/src/internal/download/apiService.ts:7`); the C# SDK and
/// the fork list 10 (`RevisionReader.DefaultBlockPageSize`), D135.
pub const BLOCK_PAGE_SIZE: u32 = 20;

/// How long a listed block's storage target is used. The storage token
/// expires 30 minutes after it is issued (`docs/sdk-patches.md`, P2); an
/// expired one is also refreshed by the fork's downloader on a 404.
pub const BLOCK_TARGET_TTL: Duration = Duration::from_secs(600);

/// Revisions kept before idle ones are dropped.
const REVISIONS_KEPT: usize = 256;

/// One listed block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed<T> {
    /// 1-based.
    pub index: u32,
    /// The listed hash of the encrypted block.
    pub hash: Vec<u8>,
    /// Where to download it.
    pub target: T,
}

/// One answer of a block listing.
pub struct Page<T, M> {
    /// From the index asked for, sorted, without a gap; none past the end.
    pub blocks: Vec<Listed<T>>,
    /// What every answer carries for the manifest (the revision).
    pub meta: M,
}

/// Lists a revision's blocks and authenticates their hashes.
#[async_trait::async_trait]
pub trait PageSource: Send + Sync {
    type Target: Clone + Send + Sync + 'static;
    type Meta: Send + Sync + 'static;

    /// The blocks from `from` on, at most `size` of them.
    async fn page(&self, from: u32, size: u32) -> Result<Page<Self::Target, Self::Meta>>;

    /// Authenticates every block's hash, block 1 first, with `meta` of any
    /// page of the listing.
    async fn verify(&self, meta: &Self::Meta, hashes: &[&[u8]]) -> Result<()>;
}

struct Entry<T> {
    listed: Instant,
    hash: Vec<u8>,
    target: T,
}

/// What is known of one revision's block list.
pub struct Revision<T, M> {
    /// The number of blocks, when known: the claimed block sizes, or a
    /// whole listing.
    count: parking_lot::Mutex<Option<u32>>,
    entries: parking_lot::Mutex<HashMap<u32, Entry<T>>>,
    /// One per page, held while the page is listed.
    pages: parking_lot::Mutex<HashMap<u32, Arc<tokio::sync::Mutex<()>>>>,
    meta: parking_lot::Mutex<Option<Arc<M>>>,
    /// Every block's hash, authenticated by the manifest.
    verified: tokio::sync::OnceCell<Arc<Vec<Vec<u8>>>>,
}

impl<T: Clone, M> Revision<T, M> {
    fn new() -> Self {
        Self {
            count: parking_lot::Mutex::new(None),
            entries: parking_lot::Mutex::default(),
            pages: parking_lot::Mutex::default(),
            meta: parking_lot::Mutex::new(None),
            verified: tokio::sync::OnceCell::new(),
        }
    }

    /// The number of blocks, from the claimed block sizes.
    pub fn set_count(&self, count: u32) {
        *self.count.lock() = Some(count);
    }

    pub fn count(&self) -> Option<u32> {
        *self.count.lock()
    }

    /// The meta of the last answer, if any page was listed.
    pub fn meta(&self) -> Option<Arc<M>> {
        self.meta.lock().clone()
    }

    fn fresh(&self, index: u32, ttl: Duration) -> Option<Listed<T>> {
        let entries = self.entries.lock();
        let entry = entries.get(&index)?;
        (entry.listed.elapsed() < ttl).then(|| Listed {
            index,
            hash: entry.hash.clone(),
            target: entry.target.clone(),
        })
    }

    fn page_lock(&self, page: u32) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(self.pages.lock().entry(page).or_default())
    }

    /// Keeps a page's blocks; after the manifest verified, their hashes
    /// must be the verified ones.
    fn keep(&self, page: Page<T, M>) -> Result<usize> {
        if let Some(verified) = self.verified.get() {
            for block in &page.blocks {
                let known = verified.get(block.index as usize - 1);
                if known != Some(&block.hash) {
                    return Err(BackendError::Integrity(format!(
                        "block {} is listed with another hash than the verified listing",
                        block.index
                    )));
                }
            }
        }
        let listed = Instant::now();
        let count = page.blocks.len();
        {
            let mut entries = self.entries.lock();
            for block in page.blocks {
                entries.insert(
                    block.index,
                    Entry {
                        listed,
                        hash: block.hash,
                        target: block.target,
                    },
                );
            }
        }
        *self.meta.lock() = Some(Arc::new(page.meta));
        Ok(count)
    }
}

type RevisionKey = (LinkId, RevisionId);

/// The block lists of the revisions a backend reads.
pub struct BlockLists<T, M> {
    revisions: parking_lot::Mutex<HashMap<RevisionKey, Arc<Revision<T, M>>>>,
    ttl: Duration,
}

impl<T: Clone + Send + Sync + 'static, M: Send + Sync + 'static> Default for BlockLists<T, M> {
    fn default() -> Self {
        Self::new(BLOCK_TARGET_TTL)
    }
}

impl<T: Clone + Send + Sync + 'static, M: Send + Sync + 'static> BlockLists<T, M> {
    pub fn new(ttl: Duration) -> Self {
        Self {
            revisions: parking_lot::Mutex::default(),
            ttl,
        }
    }

    /// The revision's list, kept while in use; idle ones beyond
    /// [`REVISIONS_KEPT`] are dropped, and listed again when read.
    pub fn revision(&self, file: &LinkId, revision: &RevisionId) -> Arc<Revision<T, M>> {
        let mut revisions = self.revisions.lock();
        if revisions.len() >= REVISIONS_KEPT {
            revisions.retain(|_, kept| Arc::strong_count(kept) > 1);
        }
        Arc::clone(
            revisions
                .entry((file.clone(), revision.clone()))
                .or_insert_with(|| Arc::new(Revision::new())),
        )
    }

    /// Block `index`'s target and listed hash: its page is listed when the
    /// block was not, or its target expired. A short page in the middle of
    /// the listing (a server that answers fewer blocks than asked for) is
    /// followed by a listing from the block itself.
    pub async fn target<S>(
        &self,
        revision: &Revision<T, M>,
        source: &S,
        index: u32,
    ) -> Result<Listed<T>>
    where
        S: PageSource<Target = T, Meta = M> + ?Sized,
    {
        if index == 0 {
            return Err(BackendError::NotFound);
        }
        if let Some(listed) = revision.fresh(index, self.ttl) {
            return Ok(listed);
        }
        let page = (index - 1) / BLOCK_PAGE_SIZE;
        let lock = revision.page_lock(page);
        let _listing = lock.lock().await;
        if let Some(listed) = revision.fresh(index, self.ttl) {
            return Ok(listed);
        }
        let from = page * BLOCK_PAGE_SIZE + 1;
        let started = std::time::Instant::now();
        let listed = revision.keep(source.page(from, BLOCK_PAGE_SIZE).await?)?;
        tracing::debug!(from, listed, took = ?started.elapsed(), "block page listed");
        if let Some(found) = revision.fresh(index, self.ttl) {
            return Ok(found);
        }
        if listed > 0 && index > from {
            revision.keep(source.page(index, BLOCK_PAGE_SIZE).await?)?;
            if let Some(found) = revision.fresh(index, self.ttl) {
                return Ok(found);
            }
        }
        Err(BackendError::NotFound)
    }

    /// Every block's hash, authenticated by the manifest: listed the first
    /// time page by page, in order, one at a time, taking the blocks known
    /// already, then verified. A listing that fails is listed again at the
    /// next call. The listing ends at the claimed number of blocks, or
    /// without it at a page shorter than [`BLOCK_PAGE_SIZE`]; a listing cut
    /// short fails the manifest.
    pub async fn verified<S>(
        &self,
        revision: &Revision<T, M>,
        source: &S,
    ) -> Result<Arc<Vec<Vec<u8>>>>
    where
        S: PageSource<Target = T, Meta = M> + ?Sized,
    {
        revision
            .verified
            .get_or_try_init(|| async {
                let count = revision.count();
                let mut hashes: Vec<Vec<u8>> = Vec::new();
                let mut next = 1u32;
                loop {
                    if count.is_some_and(|count| next > count) {
                        break;
                    }
                    let known = revision.entries.lock().get(&next).map(|e| e.hash.clone());
                    if let Some(hash) = known {
                        hashes.push(hash);
                        next += 1;
                        continue;
                    }
                    // A read may be listing this page: wait for it.
                    let lock = revision.page_lock((next - 1) / BLOCK_PAGE_SIZE);
                    let _listing = lock.lock().await;
                    if revision.entries.lock().contains_key(&next) {
                        continue;
                    }
                    let listed = revision.keep(source.page(next, BLOCK_PAGE_SIZE).await?)?;
                    if listed == 0 {
                        break;
                    }
                    let known = revision.entries.lock().contains_key(&next);
                    if !known {
                        return Err(BackendError::Integrity(format!(
                            "the block listing has no block {next}"
                        )));
                    }
                    if count.is_none() && listed < BLOCK_PAGE_SIZE as usize {
                        // The rest of this page is known: take it, then stop.
                        let entries = revision.entries.lock();
                        while let Some(entry) = entries.get(&next) {
                            hashes.push(entry.hash.clone());
                            next += 1;
                        }
                        break;
                    }
                }
                if count.is_some_and(|count| hashes.len() != count as usize) {
                    return Err(BackendError::Integrity(format!(
                        "the block listing has {} blocks, the claimed sizes {}",
                        hashes.len(),
                        count.unwrap_or(0)
                    )));
                }
                let meta = match revision.meta() {
                    Some(meta) => meta,
                    // Every block was known, none listed by this process:
                    // the first page again for the manifest.
                    None => {
                        revision.keep(source.page(1, BLOCK_PAGE_SIZE).await?)?;
                        revision.meta().expect("kept a page")
                    }
                };
                let refs: Vec<&[u8]> = hashes.iter().map(Vec::as_slice).collect();
                source.verify(&meta, &refs).await?;
                revision.set_count(hashes.len() as u32);
                Ok(Arc::new(hashes))
            })
            .await
            .map(Arc::clone)
    }

    /// Downloads block `index` with `download`, beside the manifest's
    /// verification on the revision's first read; the block counts only
    /// when the manifest verified and lists the hash it was checked with.
    pub async fn fetch<S, F, Fut, R>(
        &self,
        revision: &Revision<T, M>,
        source: &S,
        index: u32,
        download: F,
    ) -> Result<R>
    where
        S: PageSource<Target = T, Meta = M> + ?Sized,
        F: FnOnce(Listed<T>) -> Fut,
        Fut: std::future::Future<Output = Result<R>>,
    {
        let listed = self.target(revision, source, index).await?;
        let hash = listed.hash.clone();
        let (read, verified) = tokio::join!(download(listed), self.verified(revision, source));
        let verified = verified?;
        if verified.get(index as usize - 1) != Some(&hash) {
            return Err(BackendError::Integrity(format!(
                "block {index} was read with another hash than the verified listing"
            )));
        }
        read
    }

    /// Puts blocks in a revision's list as listed now; `verified`: their
    /// hashes are the whole, authenticated listing (tests).
    #[cfg(test)]
    pub(crate) fn seed(
        &self,
        file: &LinkId,
        revision: &RevisionId,
        blocks: Vec<Listed<T>>,
        meta: M,
        verified: bool,
    ) {
        let kept = self.revision(file, revision);
        let hashes: Vec<Vec<u8>> = blocks.iter().map(|b| b.hash.clone()).collect();
        kept.keep(Page { blocks, meta }).expect("seed");
        if verified {
            kept.set_count(hashes.len() as u32);
            assert!(
                kept.verified.set(Arc::new(hashes)).is_ok(),
                "not verified yet"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A revision of `count` blocks; `cap` blocks per answer at most.
    struct Source {
        count: u32,
        cap: u32,
        pages: parking_lot::Mutex<Vec<(u32, u32)>>,
        verifies: AtomicUsize,
        bad_manifest: bool,
        hash_of: fn(u32) -> Vec<u8>,
    }

    fn hash(index: u32) -> Vec<u8> {
        vec![index as u8; 4]
    }

    impl Source {
        fn new(count: u32) -> Self {
            Self {
                count,
                cap: BLOCK_PAGE_SIZE,
                pages: parking_lot::Mutex::default(),
                verifies: AtomicUsize::new(0),
                bad_manifest: false,
                hash_of: hash,
            }
        }

        fn pages(&self) -> Vec<(u32, u32)> {
            self.pages.lock().clone()
        }
    }

    #[async_trait::async_trait]
    impl PageSource for Source {
        type Target = String;
        type Meta = u32;

        async fn page(&self, from: u32, size: u32) -> Result<Page<String, u32>> {
            self.pages.lock().push((from, size));
            let end = (from + size.min(self.cap)).min(self.count + 1);
            Ok(Page {
                blocks: (from..end)
                    .map(|index| Listed {
                        index,
                        hash: (self.hash_of)(index),
                        target: format!("url-{index}"),
                    })
                    .collect(),
                meta: self.count,
            })
        }

        async fn verify(&self, meta: &u32, hashes: &[&[u8]]) -> Result<()> {
            self.verifies.fetch_add(1, Ordering::SeqCst);
            let whole = hashes.len() as u32 == *meta
                && hashes
                    .iter()
                    .enumerate()
                    .all(|(i, h)| *h == hash(i as u32 + 1));
            if self.bad_manifest || !whole {
                return Err(BackendError::Integrity("manifest".into()));
            }
            Ok(())
        }
    }

    fn ids() -> (LinkId, RevisionId) {
        (LinkId::new("file"), RevisionId::new("rev"))
    }

    /// A read lists its block's page only; the manifest lists the others in
    /// order, taking the read's page as it is; later reads in a listed page
    /// list nothing.
    #[tokio::test]
    async fn a_read_lists_its_page_and_the_manifest_the_others() {
        let lists = BlockLists::<String, u32>::default();
        let source = Source::new(50);
        let (file, rev) = ids();
        let revision = lists.revision(&file, &rev);
        revision.set_count(50);
        let got = lists
            .fetch(
                &revision,
                &source,
                45,
                |listed| async move { Ok(listed.target) },
            )
            .await
            .expect("read");
        assert_eq!(got, "url-45");
        assert_eq!(source.pages(), [(41, 20), (1, 20), (21, 20)]);
        assert_eq!(source.verifies.load(Ordering::SeqCst), 1);
        lists
            .fetch(&revision, &source, 3, |_| async { Ok(()) })
            .await
            .expect("read");
        assert_eq!(source.pages().len(), 3, "listed and verified already");
    }

    /// Without the claimed count, the listing ends at a short page.
    #[tokio::test]
    async fn without_a_count_the_listing_ends_at_a_short_page() {
        let lists = BlockLists::<String, u32>::default();
        let source = Source::new(25);
        let (file, rev) = ids();
        let revision = lists.revision(&file, &rev);
        let hashes = lists.verified(&revision, &source).await.expect("verified");
        assert_eq!(hashes.len(), 25);
        assert_eq!(source.pages(), [(1, 20), (21, 20)]);
        assert_eq!(revision.count(), Some(25));
    }

    /// An expired target is listed again with its page alone, and checked
    /// against the verified hashes.
    #[tokio::test(start_paused = true)]
    async fn an_expired_page_is_listed_again_alone() {
        let lists = BlockLists::<String, u32>::new(Duration::from_secs(60));
        let source = Source::new(50);
        let (file, rev) = ids();
        let revision = lists.revision(&file, &rev);
        revision.set_count(50);
        lists.verified(&revision, &source).await.expect("verified");
        assert_eq!(source.pages().len(), 3);
        tokio::time::advance(Duration::from_secs(61)).await;
        let listed = lists.target(&revision, &source, 30).await.expect("target");
        assert_eq!(listed.target, "url-30");
        assert_eq!(source.pages()[3..], [(21, 20)]);
        assert_eq!(source.verifies.load(Ordering::SeqCst), 1);
        // Another block of that page: listed already.
        lists.target(&revision, &source, 40).await.expect("target");
        assert_eq!(source.pages().len(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn a_page_listed_again_with_other_hashes_fails() {
        let lists = BlockLists::<String, u32>::new(Duration::from_secs(60));
        let mut source = Source::new(5);
        let (file, rev) = ids();
        let revision = lists.revision(&file, &rev);
        lists.verified(&revision, &source).await.expect("verified");
        tokio::time::advance(Duration::from_secs(61)).await;
        source.hash_of = |index| vec![index as u8 + 1; 4];
        let error = lists.target(&revision, &source, 2).await.unwrap_err();
        assert!(matches!(error, BackendError::Integrity(_)), "{error:?}");
    }

    /// A manifest that does not verify fails the read, and the next read
    /// tries again.
    #[tokio::test]
    async fn a_failed_manifest_fails_the_read() {
        let lists = BlockLists::<String, u32>::default();
        let mut source = Source::new(3);
        source.bad_manifest = true;
        let (file, rev) = ids();
        let revision = lists.revision(&file, &rev);
        let read = lists
            .fetch(&revision, &source, 1, |_| async { Ok(()) })
            .await;
        assert!(matches!(read, Err(BackendError::Integrity(_))), "{read:?}");
        source.bad_manifest = false;
        lists
            .fetch(&revision, &source, 1, |_| async { Ok(()) })
            .await
            .expect("verified now");
        assert_eq!(source.verifies.load(Ordering::SeqCst), 2);
    }

    /// A server that answers fewer blocks than asked for: a read past the
    /// short page lists from its block, and the manifest pages on to the
    /// claimed count.
    #[tokio::test]
    async fn a_capped_page_size_still_finds_every_block() {
        let lists = BlockLists::<String, u32>::default();
        let mut source = Source::new(30);
        source.cap = 10;
        let (file, rev) = ids();
        let revision = lists.revision(&file, &rev);
        revision.set_count(30);
        let listed = lists.target(&revision, &source, 15).await.expect("target");
        assert_eq!(listed.target, "url-15");
        assert_eq!(source.pages(), [(1, 20), (15, 20)]);
        lists.verified(&revision, &source).await.expect("verified");
        assert_eq!(source.pages()[2..], [(11, 20), (25, 20)]);
    }

    /// Without the claimed count a capped server cuts the listing short:
    /// the manifest fails, nothing is read wrongly.
    #[tokio::test]
    async fn a_capped_listing_without_a_count_fails_the_manifest() {
        let lists = BlockLists::<String, u32>::default();
        let mut source = Source::new(30);
        source.cap = 10;
        let (file, rev) = ids();
        let revision = lists.revision(&file, &rev);
        let error = lists.verified(&revision, &source).await.unwrap_err();
        assert!(matches!(error, BackendError::Integrity(_)), "{error:?}");
    }

    #[tokio::test]
    async fn a_block_past_the_end_is_not_found() {
        let lists = BlockLists::<String, u32>::default();
        let source = Source::new(3);
        let (file, rev) = ids();
        let revision = lists.revision(&file, &rev);
        for index in [0, 4, 30] {
            let error = lists.target(&revision, &source, index).await.unwrap_err();
            assert_eq!(error, BackendError::NotFound, "block {index}");
        }
    }

    /// A count from the claimed sizes that the listing does not match.
    #[tokio::test]
    async fn a_listing_longer_or_shorter_than_the_sizes_fails() {
        for claimed in [2, 4] {
            let lists = BlockLists::<String, u32>::default();
            let source = Source::new(3);
            let (file, rev) = ids();
            let revision = lists.revision(&file, &rev);
            revision.set_count(claimed);
            let error = lists.verified(&revision, &source).await.unwrap_err();
            assert!(
                matches!(error, BackendError::Integrity(_)),
                "{claimed}: {error:?}"
            );
        }
    }
}
