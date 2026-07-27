use knot_cob::{ChangeId, CobError};
use knot_git::GitError;
use knot_types::TypeName;

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error(transparent)]
    Cob(#[from] CobError),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("expected at most one {type_name} object, found {count}")]
    Ambiguous { type_name: TypeName, count: usize },
    #[error("change {change} in {type_name} projection does not decode: {reason}")]
    Decode {
        change: ChangeId,
        type_name: TypeName,
        reason: String,
    },
    #[error("change {change} is {found} change in {expected} projection")]
    UnexpectedType {
        change: ChangeId,
        expected: TypeName,
        found: TypeName,
    },
}
