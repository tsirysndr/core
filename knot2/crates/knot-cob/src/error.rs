use knot_types::{ChangeId, CobId, Oid, TypeName};

#[derive(Debug, thiserror::Error)]
pub enum PayloadError {
    #[error("dag-cbor encode failed: {0}")]
    Encode(String),
    #[error("dag-cbor decode failed: {0}")]
    Decode(String),
}

#[derive(Debug, thiserror::Error)]
pub enum CobError {
    #[error(transparent)]
    Git(#[from] knot_git::GitError),
    #[error(transparent)]
    Payload(#[from] PayloadError),
    #[error("object {0} does not exist")]
    NoSuchObject(CobId),
    #[error("object {0} tip does not descend from its root change")]
    DetachedTip(CobId),
    #[error("object {object} tip does not descend from indexed tip {since}")]
    DivergedTip { object: CobId, since: ChangeId },
    #[error("object {0} is not rooted at genesis change")]
    RootNotGenesis(CobId),
    #[error("object {object} contains second parentless change {stray}")]
    MultipleRoots { object: CobId, stray: ChangeId },
    #[error("authoritative object {object} has forked history at merge change {change}")]
    ForkedHistory { object: CobId, change: ChangeId },
    #[error("change {change} is not validly signed by the owning identity")]
    UnverifiedChange { change: ChangeId },
    #[error("concurrent write moved object {object} past expected tip {expected}")]
    StaleTip { object: CobId, expected: ChangeId },
    #[error("object {0} exceeded its compare-and-swap retry budget under contention")]
    Contended(CobId),
    #[error("change {oid} is malformed: {reason}")]
    MalformedChange { oid: Oid, reason: String },
    #[error("change {change} payload does not decode: {reason}")]
    UndecodableChange { change: ChangeId, reason: String },
    #[error("change {change} is a {found} change in {expected} object")]
    UnexpectedChangeType {
        change: ChangeId,
        expected: TypeName,
        found: TypeName,
    },
    #[error("produced unverifiable signature for {0} change")]
    SelfCheck(TypeName),
    #[error("change graph for {0} exceeds load bound")]
    HistoryTooLong(CobId),
    #[error("'{0}' is not usable collaborative-object ref name")]
    RefName(String),
    #[error("writing git object failed: {0}")]
    Write(String),
}
