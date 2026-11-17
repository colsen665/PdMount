//! Block keys, priorities and the block layout of a revision (DESIGN.md §6.1).

use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockKey {
    pub link: LinkId,
    pub rev: RevisionId,
    /// 1-based, as on the server.
    pub index: u32,
}

impl BlockKey {
    pub fn new(link: &LinkId, rev: &RevisionId, index: u32) -> Self {
        Self {
            link: link.clone(),
            rev: rev.clone(),
            index,
        }
    }

    /// Bytes used as file name source and AEAD associated data.
    pub(super) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.link.0.len() + self.rev.0.len() + 6);
        out.extend_from_slice(self.link.0.as_bytes());
        out.push(0);
        out.extend_from_slice(self.rev.0.as_bytes());
        out.push(0);
        out.extend_from_slice(&self.index.to_le_bytes());
        out
    }
}

/// Priority classes with separate download permits (§6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Interactive,
    Readahead,
    Prefetch,
    /// A scan read (D111): its blocks count against the scan share.
    Scan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockSpan {
    pub index: u32,
    pub offset: u64,
    pub len: u32,
}

/// Queues the rows of a revision's layout (`revision_blocks`).
pub(super) fn store_layout(store: &Store, link: &LinkId, rev: &RevisionId, layout: &Layout) {
    let rows: Vec<(u32, u32)> = layout.blocks.iter().map(|b| (b.index, b.len)).collect();
    let (link, rev) = (link.0.clone(), rev.0.clone());
    store.write(move |conn| {
        let mut statement = conn.prepare_cached(
            "INSERT OR REPLACE INTO revision_blocks(link_id, rev, idx, plain_size) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (index, len) in &rows {
            statement.execute((&link, &rev, index, len))?;
        }
        Ok(())
    });
}

/// Where each block of a revision sits in the file (§6.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub blocks: Vec<BlockSpan>,
}

impl Layout {
    /// Uses the declared plaintext sizes when every block has one; otherwise
    /// assumes 4 MiB blocks. Either way the blocks must add up to
    /// `file_size`. Declared sizes are the uploader's claim (§6.1): a block
    /// above 4 MiB, an empty block before the last, or a sum other than the
    /// file size is an integrity failure, so no claim sizes a buffer. No
    /// blocks at all take the second path, so they hold only an empty file.
    pub fn new(infos: &[BlockInfo], file_size: u64) -> Result<Self> {
        let integrity =
            |message: String| CoreError::Backend(pdmount_backend::BackendError::Integrity(message));
        let mut blocks = Vec::with_capacity(infos.len());
        let mut offset = 0u64;
        if !infos.is_empty() && infos.iter().all(|info| info.plain_size.is_some()) {
            for (position, info) in infos.iter().enumerate() {
                let len = info.plain_size.unwrap_or(0);
                let last = position + 1 == infos.len();
                if u64::from(len) > BLOCK_SIZE || (len == 0 && !last) {
                    return Err(integrity(format!(
                        "block {} declares {len} bytes",
                        info.index
                    )));
                }
                blocks.push(BlockSpan {
                    index: info.index,
                    offset,
                    len,
                });
                offset += u64::from(len);
            }
            if offset != file_size {
                return Err(integrity(format!(
                    "block sizes add up to {offset} bytes, the file has {file_size}"
                )));
            }
            return Ok(Self { blocks });
        }
        let count = infos.len() as u64;
        let full = count.saturating_sub(1) * BLOCK_SIZE;
        let last = file_size.checked_sub(full).filter(|&last| {
            (count == 0 && last == 0) || (count > 0 && last > 0 && last <= BLOCK_SIZE)
        });
        let Some(last) = last else {
            return Err(integrity(format!(
                "{count} blocks of at most 4 MiB cannot hold {file_size} bytes"
            )));
        };
        for (position, info) in infos.iter().enumerate() {
            let len = if position + 1 == infos.len() {
                last
            } else {
                BLOCK_SIZE
            };
            blocks.push(BlockSpan {
                index: info.index,
                offset,
                len: len as u32,
            });
            offset += len;
        }
        Ok(Self { blocks })
    }

    pub fn size(&self) -> u64 {
        self.blocks
            .last()
            .map_or(0, |b| b.offset + u64::from(b.len))
    }

    /// Blocks overlapping `[offset, offset + len)`.
    pub fn spans(&self, offset: u64, len: u64) -> &[BlockSpan] {
        let end = offset.saturating_add(len);
        let first = self
            .blocks
            .partition_point(|b| b.offset + u64::from(b.len) <= offset);
        let last = self.blocks.partition_point(|b| b.offset < end);
        &self.blocks[first..last.max(first)]
    }

    /// The block after `index`, if any.
    pub fn next_after(&self, index: u32) -> Option<&BlockSpan> {
        let position = self.blocks.iter().position(|b| b.index == index)?;
        self.blocks.get(position + 1)
    }
}
