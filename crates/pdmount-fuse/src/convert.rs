//! Conversions between the engine's types and the kernel's, and small reply
//! helpers shared by the callbacks.

use super::BLKSIZE;
use fuser::{
    Errno, FileAttr, FileType, INodeNo, OpenAccMode, OpenFlags, ReplyDirectory, ReplyEmpty,
    ReplyXattr, TimeOrNow,
};
use pdmount_backend::NodeKind;
use pdmount_core::engine::OpenMode;
use pdmount_core::errors::{CoreError, Result};
use pdmount_core::vfs::{Attr, DirEntry};
use std::time::SystemTime;

/// Answers an empty reply with a result.
pub(super) fn reply_empty(reply: ReplyEmpty) -> impl FnOnce(Result<()>) + Send + 'static {
    move |result| match result {
        Ok(()) => reply.ok(),
        Err(error) => reply.error(errno(&error)),
    }
}

/// The size protocol of `getxattr` and `listxattr`: a zero `size` asks for
/// the length, a smaller buffer is ERANGE.
pub(super) fn xattr_reply(reply: ReplyXattr, data: &[u8], size: u32) {
    if size == 0 {
        reply.size(data.len() as u32);
    } else if (size as usize) < data.len() {
        reply.error(Errno::ERANGE);
    } else {
        reply.data(data);
    }
}

pub(super) fn errno(error: &CoreError) -> Errno {
    Errno::from_i32(error.errno())
}

pub(super) fn file_attr(attr: &Attr, uid: u32, gid: u32) -> FileAttr {
    FileAttr {
        ino: INodeNo(attr.ino),
        size: attr.size,
        blocks: attr.size.div_ceil(512),
        atime: attr.atime,
        mtime: attr.mtime,
        ctime: attr.ctime,
        crtime: attr.mtime,
        kind: file_type(attr.kind),
        perm: attr.perm as u16,
        nlink: attr.nlink,
        uid,
        gid,
        rdev: 0,
        blksize: BLKSIZE,
        flags: 0,
    }
}

pub(super) fn file_type(kind: NodeKind) -> FileType {
    match kind {
        NodeKind::File => FileType::RegularFile,
        NodeKind::Folder => FileType::Directory,
    }
}

/// Adds directory entries from `offset` on; returns when the buffer is full.
pub(super) fn fill_dir(entries: &[DirEntry], offset: u64, reply: &mut ReplyDirectory) {
    for (index, entry) in entries.iter().enumerate().skip(offset as usize) {
        if reply.add(
            INodeNo(entry.ino),
            index as u64 + 1,
            file_type(entry.kind),
            &entry.name,
        ) {
            break;
        }
    }
}

pub(super) fn open_mode(flags: i32) -> OpenMode {
    let access = OpenFlags(flags).acc_mode();
    OpenMode {
        read: access != OpenAccMode::O_WRONLY,
        write: access != OpenAccMode::O_RDONLY,
        truncate: flags & nix::libc::O_TRUNC != 0,
    }
}

pub(super) fn time(value: TimeOrNow) -> SystemTime {
    match value {
        TimeOrNow::SpecificTime(time) => time,
        TimeOrNow::Now => SystemTime::now(),
    }
}

/// Answers a request that would change the top level: it is read-only.
pub(super) fn top_read_only() -> Errno {
    Errno::EROFS
}
