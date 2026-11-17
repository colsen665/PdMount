//! What a read returns: [`ReadFrom`], [`ReadData`] and the [`ReadBuf`] that assembles it.

use super::*;

/// Blocks a read holds at once: readers copy each block out as it arrives
/// and drop it, so a read over many small blocks does not pin a pool class
/// (§6.2). Two, so a read across a block boundary fetches both together.
pub const READ_BLOCKS_HELD: usize = 2;

/// The part of block `span` (`data_len` bytes) inside `[offset, offset +
/// len)`, relative to the block.
pub(super) fn span_range(span: &BlockSpan, data_len: usize, offset: u64, len: u64) -> Range<usize> {
    let end = offset + len;
    let start = offset.max(span.offset) - span.offset;
    let stop = (end.min(span.offset + u64::from(span.len)) - span.offset).min(data_len as u64);
    start as usize..stop.max(start) as usize
}

/// Where a read served without the network found its bytes; the FUSE
/// layer times it under this (§5.8). Ordered by cost: a read across blocks
/// counts as its slowest source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReadFrom {
    Ram,
    Disk,
    Staging,
}

/// The bytes of a read (§6.2): a slice of a pool slot (the one block the
/// read lies in, or the scratch slot a read across blocks was assembled
/// in), or a heap buffer (staging, fallbacks).
#[derive(Debug, Clone)]
pub enum ReadData {
    Slice(Bytes),
    Owned(Vec<u8>),
}

impl ReadData {
    pub fn empty() -> Self {
        Self::Owned(Vec::new())
    }

    /// `[offset, offset + len)` of the one block `span` it lies in, without
    /// copying.
    pub fn of_block(span: &BlockSpan, data: Bytes, offset: u64, len: u64) -> Self {
        Self::Slice(data.slice(span_range(span, data.len(), offset, len)))
    }

    pub fn into_vec(self) -> Vec<u8> {
        match self {
            Self::Slice(data) => data.to_vec(),
            Self::Owned(data) => data,
        }
    }
}

impl std::ops::Deref for ReadData {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Slice(data) => data,
            Self::Owned(data) => data,
        }
    }
}

impl<T: AsRef<[u8]> + ?Sized> PartialEq<T> for ReadData {
    fn eq(&self, other: &T) -> bool {
        **self == *other.as_ref()
    }
}

/// A read across blocks, assembled in a scratch slot (or a heap buffer)
/// one block at a time.
pub struct ReadBuf {
    pub(super) buffer: Buffer,
    pub(super) filled: usize,
    pub(super) offset: u64,
    pub(super) len: u64,
}

impl ReadBuf {
    pub(super) fn new(buffer: Buffer, offset: u64, len: u64) -> Self {
        Self {
            buffer,
            filled: 0,
            offset,
            len,
        }
    }

    /// Appends the part of block `span` (`data`) inside the read. Blocks
    /// come in order.
    pub fn append(&mut self, span: &BlockSpan, data: &[u8]) {
        let range = span_range(span, data.len(), self.offset, self.len);
        let start = PLAIN_START + self.filled;
        self.buffer.as_mut()[start..start + range.len()].copy_from_slice(&data[range.clone()]);
        self.filled += range.len();
    }

    pub fn finish(self) -> ReadData {
        ReadData::Slice(self.buffer.freeze(self.filled))
    }
}
