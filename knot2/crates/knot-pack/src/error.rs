use std::fmt;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackLimit {
    Objects,
    ObjectBytes,
    TotalBytes,
    DeltaDepth,
}

impl fmt::Display for PackLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PackLimit::Objects => "object count",
            PackLimit::ObjectBytes => "per-object size",
            PackLimit::TotalBytes => "total decompressed size",
            PackLimit::DeltaDepth => "delta chain depth",
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PackError {
    #[error("repository not found")]
    NotFound,
    #[error("repository index is warming, retry shortly")]
    Unavailable,
    #[error("invalid request path: {0}")]
    BadPath(String),
    #[error("unsupported service")]
    UnsupportedService,
    #[error("push is served over SSH, not HTTP")]
    PushOverSsh,
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("unsupported request content-encoding: {0}")]
    UnsupportedEncoding(String),
    #[error("pkt-line: {0}")]
    PktLine(#[from] std::io::Error),
    #[error("pack: {0}")]
    Pack(String),
    #[error("pack exceeds {0} limit")]
    LimitExceeded(PackLimit),
    #[error("upload-pack selection exceeded its object-set limit")]
    SelectionTooLarge,
    #[error("upload-pack selection exceeded its time budget")]
    SelectionTimeout,
    #[error("insufficient memory to ingest this push, retry when the server is less busy")]
    InsufficientMemory,
    #[error(transparent)]
    Git(knot_git::GitError),
}

impl From<knot_git::GitError> for PackError {
    fn from(error: knot_git::GitError) -> Self {
        use knot_git::SelectionLimit;
        match error {
            knot_git::GitError::Selection(SelectionLimit::Objects) => PackError::SelectionTooLarge,
            knot_git::GitError::Selection(SelectionLimit::Time) => PackError::SelectionTimeout,
            other => PackError::Git(other),
        }
    }
}

impl PackError {
    pub fn http_status(&self) -> StatusCode {
        match self {
            PackError::NotFound => StatusCode::NOT_FOUND,
            PackError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            PackError::PushOverSsh => StatusCode::FORBIDDEN,
            PackError::BadPath(_) | PackError::UnsupportedService => StatusCode::BAD_REQUEST,
            PackError::Protocol(_) | PackError::PktLine(_) => StatusCode::BAD_REQUEST,
            PackError::UnsupportedEncoding(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            PackError::LimitExceeded(_) => StatusCode::PAYLOAD_TOO_LARGE,
            PackError::SelectionTooLarge
            | PackError::SelectionTimeout
            | PackError::InsufficientMemory => StatusCode::SERVICE_UNAVAILABLE,
            PackError::Pack(_) | PackError::Git(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for PackError {
    fn into_response(self) -> Response {
        (self.http_status(), self.to_string()).into_response()
    }
}
