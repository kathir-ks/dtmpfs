use thiserror::Error;
use crate::id::NodeId;

#[derive(Debug, Error)]
pub enum DtmpfsError {
    #[error("meta server unavailable")]
    MetaUnavailable,
    #[error("store node {0:?} unavailable")]
    StoreUnavailable(NodeId),
    #[error("block generation mismatch (write rejected as stale)")]
    BlockGenerationMismatch,
    #[error("not found")]
    NotFound,
    #[error("already exists")]
    AlreadyExists,
    #[error("not a directory")]
    NotADirectory,
    #[error("is a directory")]
    IsADirectory,
    #[error("directory not empty")]
    NotEmpty,
    #[error("permission denied")]
    PermissionDenied,
    #[error("resource exhausted")]
    ResourceExhausted,
    #[error("invalid argument")]
    InvalidArgument,
    #[error("unauthenticated")]
    Unauthenticated,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("rpc: {0}")]
    Rpc(#[from] tonic::Status),
}

impl From<DtmpfsError> for libc::c_int {
    fn from(e: DtmpfsError) -> libc::c_int {
        use DtmpfsError::*;
        match e {
            NotFound                           => libc::ENOENT,
            AlreadyExists                      => libc::EEXIST,
            NotADirectory                      => libc::ENOTDIR,
            IsADirectory                       => libc::EISDIR,
            NotEmpty                           => libc::ENOTEMPTY,
            PermissionDenied | Unauthenticated => libc::EACCES,
            ResourceExhausted                  => libc::ENOSPC,
            InvalidArgument                    => libc::EINVAL,
            MetaUnavailable | StoreUnavailable(_)
            | BlockGenerationMismatch | Io(_) | Rpc(_) => libc::EIO,
        }
    }
}

impl DtmpfsError {
    pub fn from_status(s: tonic::Status, node: Option<NodeId>) -> DtmpfsError {
        use tonic::Code::*;
        match s.code() {
            InvalidArgument | OutOfRange => DtmpfsError::InvalidArgument,
            NotFound                     => DtmpfsError::NotFound,
            AlreadyExists                => DtmpfsError::AlreadyExists,
            PermissionDenied             => DtmpfsError::PermissionDenied,
            Unauthenticated              => DtmpfsError::Unauthenticated,
            ResourceExhausted            => DtmpfsError::ResourceExhausted,
            FailedPrecondition           => DtmpfsError::BlockGenerationMismatch,
            Unavailable => match node {
                Some(n) => DtmpfsError::StoreUnavailable(n),
                None    => DtmpfsError::MetaUnavailable,
            },
            _ => DtmpfsError::Rpc(s),
        }
    }
}

pub type Result<T> = std::result::Result<T, DtmpfsError>;
