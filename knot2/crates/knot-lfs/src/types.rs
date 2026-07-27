use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use knot_types::RepoDid;

use crate::LfsError;

const OID_HEX_LEN: usize = 64;

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct LfsOid(String);

impl LfsOid {
    pub fn new(value: impl Into<String>) -> Result<Self, LfsError> {
        let value = value.into();
        let valid = value.len() == OID_HEX_LEN
            && value
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
        match valid {
            true => Ok(Self(value)),
            false => Err(LfsError::InvalidOid { value }),
        }
    }

    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(knot_types::lowercase_hex(&digest))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for LfsOid {
    type Err = LfsError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<String> for LfsOid {
    type Error = LfsError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<LfsOid> for String {
    fn from(oid: LfsOid) -> Self {
        oid.0
    }
}

impl fmt::Display for LfsOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for LfsOid {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct LfsSize(u64);

impl LfsSize {
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn saturating_add(self, other: LfsSize) -> LfsSize {
        LfsSize(self.0.saturating_add(other.0))
    }
}

impl fmt::Display for LfsSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
// The mass that the client says the incoming object has.
// Admission will reserve disk space
// using this before the obj bytes arrive,
// and the count of what actually showed up in the end is `LfsSize`.
pub struct ClaimedSize(u64);

impl ClaimedSize {
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn matches(self, actual: LfsSize) -> bool {
        self.0 == actual.get()
    }
}

impl fmt::Display for ClaimedSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FreeSpaceFloor(u64);

impl FreeSpaceFloor {
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for FreeSpaceFloor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoPrefix(PathBuf);

impl RepoPrefix {
    pub fn new(repo: &RepoDid) -> Result<Self, LfsError> {
        knot_git::repo_shard(repo)
            .map(Self)
            .map_err(|_| LfsError::UnsafeRepoDid {
                did: repo.as_str().to_string(),
            })
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ObjectRelPath(PathBuf);

impl ObjectRelPath {
    pub fn new(repo: &RepoDid, oid: &LfsOid) -> Result<Self, LfsError> {
        let prefix = RepoPrefix::new(repo)?;
        let hex = oid.as_str();
        Ok(Self(prefix.0.join(&hex[0..2]).join(&hex[2..4]).join(hex)))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsStorePath(PathBuf);

impl LfsStorePath {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self(root.into())
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn object_path(&self, rel: &ObjectRelPath) -> PathBuf {
        self.0.join(rel.as_path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_OID: &str = "6c17f2007cbe934aee6e309b28b2fba3c119d98be6ea4156da3aa3173456ad16";

    #[test]
    fn oid_accepts_lowercase_hex_and_round_trips_a_digest() {
        let oid = LfsOid::new(SAMPLE_OID).unwrap();
        assert_eq!(oid.as_str(), SAMPLE_OID);
        assert_eq!(oid.to_string(), SAMPLE_OID);
        assert_eq!(SAMPLE_OID.parse::<LfsOid>().unwrap(), oid);

        let from_digest = LfsOid::from_digest([0xab; 32]);
        assert_eq!(from_digest.as_str().len(), 64);
        assert_eq!(LfsOid::new(from_digest.as_str()).unwrap(), from_digest);
    }

    #[test]
    fn oid_rejects_everything_else() {
        let hostile = [
            "",
            "abc123",
            &SAMPLE_OID[..63],
            &format!("{SAMPLE_OID}0"),
            &SAMPLE_OID.to_uppercase(),
            &format!("{}g", &SAMPLE_OID[..63]),
            "../../../../../../etc/passwd0000000000000000000000000000000000000",
            "..%2f..%2f..%2f..%2f..%2fetc%2fpasswd0000000000000000000000000000",
            &format!("{}\0", &SAMPLE_OID[..63]),
        ];
        hostile.iter().for_each(|value| {
            assert!(
                matches!(LfsOid::new(*value), Err(LfsError::InvalidOid { .. })),
                "accepted {value:?}"
            );
        });
    }

    #[test]
    fn object_paths_shard_by_repo_then_oid_and_stay_under_the_root() {
        let repo = RepoDid::new("did:plc:squid").unwrap();
        let oid = LfsOid::new(SAMPLE_OID).unwrap();
        let rel = ObjectRelPath::new(&repo, &oid).unwrap();
        assert_eq!(
            rel.as_path(),
            Path::new("plc/sq/uid/6c/17").join(SAMPLE_OID)
        );

        let root = LfsStorePath::new("/srv/lfs");
        let path = root.object_path(&rel);
        assert!(path.starts_with(root.as_path()));
        assert!(
            path.components()
                .all(|part| !matches!(part, std::path::Component::ParentDir))
        );
    }
}
