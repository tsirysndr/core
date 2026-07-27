use knot_runtime::UnixMicros;
use knot_types::{AccountDid, HttpStatus, RepoDid};
use serde::Serialize;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(FNV_OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct RoundNumber(u32);

impl RoundNumber {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct OperationIndex(u32);

impl OperationIndex {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Answered { status: HttpStatus, body: u64 },
    Killed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Step {
    pub round: RoundNumber,
    pub index: OperationIndex,
    pub op: &'static str,
    pub actor: String,
    pub fault: &'static str,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoCollaborators {
    pub repo: RepoDid,
    pub subjects: Vec<AccountDid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Projection {
    pub members: Vec<AccountDid>,
    pub blocked: Vec<AccountDid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    pub round: RoundNumber,
    pub clock_micros: UnixMicros,
    pub members: Vec<AccountDid>,
    pub blocked: Vec<AccountDid>,
    pub repos: Vec<RepoDid>,
    pub collaborators: Vec<RepoCollaborators>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Trace {
    pub seed: u64,
    pub steps: Vec<Step>,
    pub snapshots: Vec<Snapshot>,
}

impl Trace {
    pub fn digest(&self) -> u64 {
        fnv1a(&serde_json::to_vec(self).expect("trace is always serializable"))
    }
}
