use std::path::PathBuf;

use crate::{ClaimedSize, FreeSpaceFloor, LfsOid, LfsSize};

#[derive(Debug, thiserror::Error)]
pub enum LfsError {
    #[error("invalid LFS oid {value:?}")]
    InvalidOid { value: String },
    #[error("unsafe repo DID {did:?}")]
    UnsafeRepoDid { did: String },
    #[error("oid mismatch, declared {declared}, computed {computed}")]
    HashMismatch { declared: LfsOid, computed: LfsOid },
    #[error("size mismatch, declared {declared}, received {received}")]
    SizeMismatch {
        declared: ClaimedSize,
        received: LfsSize,
    },
    #[error("object size {declared} exceeds limit {limit}")]
    SizeLimitExceeded {
        declared: ClaimedSize,
        limit: LfsSize,
    },
    #[error("free space {free} below floor {floor}")]
    FreeSpaceDenied {
        free: LfsSize,
        floor: FreeSpaceFloor,
    },
    #[error("object {oid} not found")]
    NotFound { oid: LfsOid },
    #[error("protocol framing fault: {detail}")]
    Framing { detail: String },
    #[error("too many {what} in one message, limit {limit}")]
    TooMany { what: &'static str, limit: usize },
    #[error("transfer channel fault")]
    Channel {
        #[source]
        source: std::io::Error,
    },
    #[error("object body read failed")]
    BodyRead {
        #[source]
        source: std::io::Error,
    },
    #[error("{op} {path} failed")]
    Io {
        op: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}
