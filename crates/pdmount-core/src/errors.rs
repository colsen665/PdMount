//! Engine errors and their errno mapping (DESIGN.md Appendix A).

use nix::libc;
use pdmount_backend::BackendError;

#[derive(Debug, Clone, thiserror::Error)]
pub enum CoreError {
    #[error("no such file or directory")]
    NotFound,
    #[error("not a directory")]
    NotADirectory,
    #[error("is a directory")]
    IsADirectory,
    #[error("read-only file system")]
    ReadOnly,
    #[error("file exists")]
    Exists,
    #[error("directory not empty")]
    NotEmpty,
    #[error("invalid argument")]
    InvalidArgument,
    #[error("operation not permitted")]
    NotPermitted,
    #[error("permission denied")]
    AccessDenied,
    #[error("no space left (quota)")]
    NoSpace,
    #[error("file too large")]
    FileTooLarge,
    /// The directory must be listed first (slow path); never reaches FUSE.
    #[error("directory {0} is not listed yet")]
    NeedsListing(u64),
    /// Base blocks must be downloaded into staging first (slow path).
    #[error("inode {0} needs materialization")]
    NeedsMaterialize(u64),
    /// No inode number is reserved; the slow path reserves more.
    #[error("no inode numbers reserved")]
    NeedsInos,
    /// A stored errno, returned as is (deferred upload errors, §7.8).
    #[error("error {0}")]
    Errno(i32),
    #[error("name is not valid UTF-8")]
    InvalidName,
    #[error("name longer than 255 bytes")]
    NameTooLong,
    #[error("deadline expired")]
    Timeout,
    #[error("backend: {0}")]
    Backend(#[from] BackendError),
    #[error("store: {0}")]
    Store(String),
    #[error("local I/O: {0}")]
    Io(String),
    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;

impl CoreError {
    pub fn errno(&self) -> i32 {
        match self {
            Self::NotFound => libc::ENOENT,
            Self::NotADirectory => libc::ENOTDIR,
            Self::IsADirectory => libc::EISDIR,
            Self::ReadOnly => libc::EROFS,
            Self::Exists => libc::EEXIST,
            Self::NotEmpty => libc::ENOTEMPTY,
            Self::InvalidArgument => libc::EINVAL,
            Self::NotPermitted => libc::EPERM,
            Self::AccessDenied => libc::EACCES,
            Self::NoSpace => libc::ENOSPC,
            Self::FileTooLarge => libc::EFBIG,
            Self::Errno(errno) => *errno,
            Self::InvalidName => libc::EILSEQ,
            Self::NameTooLong => libc::ENAMETOOLONG,
            Self::Backend(error) => backend_errno(error),
            Self::Timeout
            | Self::Store(_)
            | Self::Io(_)
            | Self::Internal(_)
            | Self::NeedsListing(_)
            | Self::NeedsMaterialize(_)
            | Self::NeedsInos => libc::EIO,
        }
    }
}

fn backend_errno(error: &BackendError) -> i32 {
    match error {
        BackendError::NotFound => libc::ENOENT,
        BackendError::AlreadyExists { .. } => libc::EEXIST,
        BackendError::PermissionDenied => libc::EACCES,
        BackendError::QuotaExceeded | BackendError::NestingTooDeep => libc::ENOSPC,
        BackendError::FileTooLarge => libc::EFBIG,
        BackendError::TooManyChildren => libc::EMLINK,
        BackendError::Unsupported(_) => libc::ENOTSUP,
        BackendError::RevisionConflict { .. }
        | BackendError::NameConflict
        | BackendError::RateLimited { .. }
        | BackendError::Transient(_)
        | BackendError::Unauthorized
        | BackendError::Integrity(_)
        | BackendError::Offline(_)
        | BackendError::Other(_) => libc::EIO,
    }
}

impl From<crate::store::StoreError> for CoreError {
    fn from(error: crate::store::StoreError) -> Self {
        Self::Store(error.to_string())
    }
}

impl From<std::io::Error> for CoreError {
    fn from(error: std::io::Error) -> Self {
        match error.raw_os_error() {
            Some(libc::ENOSPC) => Self::Errno(libc::ENOSPC),
            Some(libc::EDQUOT) => Self::Errno(libc::EDQUOT),
            _ => Self::Io(error.to_string()),
        }
    }
}

impl CoreError {
    /// Whether the request must continue on the slow path.
    pub fn is_slow(&self) -> bool {
        matches!(
            self,
            Self::NeedsListing(_) | Self::NeedsMaterialize(_) | Self::NeedsInos
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appendix_a_mapping() {
        assert_eq!(
            CoreError::from(BackendError::NotFound).errno(),
            libc::ENOENT
        );
        assert_eq!(
            CoreError::from(BackendError::QuotaExceeded).errno(),
            libc::ENOSPC
        );
        assert_eq!(
            CoreError::from(BackendError::Integrity("x".into())).errno(),
            libc::EIO
        );
        assert_eq!(CoreError::ReadOnly.errno(), libc::EROFS);
        assert_eq!(CoreError::InvalidName.errno(), libc::EILSEQ);
        assert_eq!(CoreError::NameTooLong.errno(), libc::ENAMETOOLONG);
    }
}
