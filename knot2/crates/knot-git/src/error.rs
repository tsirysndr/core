use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionLimit {
    Objects,
    Time,
}

impl std::fmt::Display for SelectionLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SelectionLimit::Objects => "object-set limit",
            SelectionLimit::Time => "wall-clock budget",
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("repository already exists at {0}")]
    AlreadyExists(PathBuf),
    #[error("open repository at {path}: {message}")]
    Open { path: PathBuf, message: String },
    #[error("create repository at {path}: {message}")]
    Create { path: PathBuf, message: String },
    #[error("remove repository at {path}: {message}")]
    Remove { path: PathBuf, message: String },
    #[error("reference {name}: {message}")]
    Reference { name: String, message: String },
    #[error("atomic ref transaction: {0}")]
    AtomicRefs(String),
    #[error("fsync {path}: {message}")]
    Fsync { path: PathBuf, message: String },
    #[error("atomic write to {path}: {message}")]
    Write { path: PathBuf, message: String },
    #[error("repo DID {0} maps to unsafe on-disk path component")]
    UnsafeRepoDid(String),
    #[error("repo DID {0} is reserved for knot meta-repo and is never served as user repo")]
    ReservedDid(String),
    #[error("{0} exceeds maximum supported depth")]
    DepthExceeded(&'static str),
    #[error("revision walk: {0}")]
    RevWalk(String),
    #[error("upload-pack selection exceeded its {0}")]
    Selection(SelectionLimit),
    #[error("object not found: {0}")]
    ObjectNotFound(knot_types::Oid),
    #[error("remove loose object {oid}: {message}")]
    RemoveObject {
        oid: knot_types::Oid,
        message: String,
    },
    #[error("object {oid} is corrupt: {message}")]
    Corrupt {
        oid: knot_types::Oid,
        message: String,
    },
    #[error("object {oid} isn't {expected}")]
    ObjectType {
        oid: knot_types::Oid,
        expected: &'static str,
    },
    #[error("decode object: {0}")]
    Decode(String),
    #[error("git backend: {0}")]
    Backend(String),
    #[error("object staging: {0}")]
    Staging(String),
    #[error("repository config at {path}: {message}")]
    Config { path: PathBuf, message: String },
    #[error("repository maintenance: {0}")]
    Maintenance(String),
}

impl From<knot_resource::FsError> for GitError {
    fn from(error: knot_resource::FsError) -> Self {
        GitError::Write {
            path: error.path,
            message: error.source.to_string(),
        }
    }
}

pub(crate) fn backend(error: impl std::fmt::Display) -> GitError {
    GitError::Backend(error.to_string())
}
