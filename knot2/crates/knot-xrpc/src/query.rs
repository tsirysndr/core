use axum::extract::{FromRequestParts, Query};
use http::request::Parts;
use knot_types::{OwnerDid, RepoDid, RepoPath, RepoRkey};
use serde::de::{self, Deserialize, DeserializeOwned, Deserializer};

use crate::error::XrpcError;

pub(crate) struct ValidatedQuery<T>(pub(crate) T);

impl<T, S> FromRequestParts<S> for ValidatedQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = XrpcError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Query::<T>::try_from_uri(&parts.uri)
            .map(|query| ValidatedQuery(query.0))
            .map_err(|rejection| XrpcError::invalid_request(rejection.body_text()))
    }
}

// Each route's default and its limit are included in the type,
// such that a limit that is ok on one endpoint
// can't be spent on another one with a lower roof.
#[derive(Clone, Copy)]
pub(crate) struct Limit<const DEFAULT: usize, const MAX: usize>(usize);

impl<const DEFAULT: usize, const MAX: usize> Limit<DEFAULT, MAX> {
    pub(crate) fn get(self) -> usize {
        self.0
    }
}

impl<const DEFAULT: usize, const MAX: usize> Default for Limit<DEFAULT, MAX> {
    fn default() -> Self {
        Limit(DEFAULT)
    }
}

impl<'de, const DEFAULT: usize, const MAX: usize> Deserialize<'de> for Limit<DEFAULT, MAX> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        if raw.is_empty() {
            return Ok(Limit(DEFAULT));
        }
        let value = raw
            .parse::<i64>()
            .map_err(|_| de::Error::custom("limit must be an integer"))?;
        Ok(Limit(usize::try_from(value).unwrap_or(0).min(MAX).max(1)))
    }
}

knot_types::scalar_newtype! {
    #[derive(Default)]
    pub(crate) struct Offset(usize);
}

impl<'de> Deserialize<'de> for Offset {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        if raw.is_empty() {
            return Ok(Offset::new(0));
        }
        raw.parse::<usize>()
            .map(Offset::new)
            .map_err(|_| de::Error::custom("cursor must be an integer"))
    }
}

knot_types::scalar_newtype! {
    pub(crate) struct Total(usize);
}

pub(crate) fn next_cursor<const DEFAULT: usize, const MAX: usize>(
    offset: Offset,
    limit: Limit<DEFAULT, MAX>,
    total: Total,
) -> Option<String> {
    offset
        .get()
        .checked_add(limit.get())
        .filter(|&end| end < total.get())
        .map(|end| end.to_string())
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Order {
    #[default]
    Desc,
    Asc,
}

impl Order {
    pub(crate) fn descending(self) -> bool {
        matches!(self, Order::Desc)
    }
}

impl<'de> Deserialize<'de> for Order {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "" | "desc" => Ok(Order::Desc),
            "asc" => Ok(Order::Asc),
            _ => Err(de::Error::custom("order must be 'asc' or 'desc'")),
        }
    }
}

pub(crate) enum RepoArg {
    Did(RepoDid),
    OwnerRkey { owner: OwnerDid, rkey: RepoRkey },
}

impl RepoArg {
    pub(crate) fn basename(&self) -> &str {
        match self {
            RepoArg::Did(did) => did.as_str(),
            RepoArg::OwnerRkey { rkey, .. } => rkey.as_str(),
        }
    }

    pub(crate) fn to_param(&self) -> String {
        match self {
            RepoArg::Did(did) => did.as_str().to_string(),
            RepoArg::OwnerRkey { owner, rkey } => format!("{}/{}", owner.as_str(), rkey.as_str()),
        }
    }
}

impl<'de> Deserialize<'de> for RepoArg {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        if !raw.starts_with("did:") {
            return Err(de::Error::custom(
                "missing or invalid repo parameter, expected repo DID",
            ));
        }
        Ok(match raw.split_once('/') {
            None => RepoArg::Did(RepoDid::new(raw).map_err(de::Error::custom)?),
            Some((owner, rkey)) => RepoArg::OwnerRkey {
                owner: OwnerDid::new(owner).map_err(de::Error::custom)?,
                rkey: RepoRkey::new(rkey).map_err(de::Error::custom)?,
            },
        })
    }
}

const MAX_REVSPEC_BYTES: usize = 4096;

#[derive(Clone, Default)]
pub(crate) struct Revspec(String);

impl Revspec {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Revspec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        match raw.len() <= MAX_REVSPEC_BYTES && !raw.chars().any(char::is_control) {
            true => Ok(Self(raw)),
            false => Err(de::Error::custom("invalid revision")),
        }
    }
}

#[derive(Default)]
pub(crate) struct BranchArg(Option<knot_types::BranchName>);

impl BranchArg {
    pub(crate) fn get(&self) -> Option<&knot_types::BranchName> {
        self.0.as_ref()
    }
}

impl<'de> serde::Deserialize<'de> for BranchArg {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)? {
            raw if raw.is_empty() => Ok(Self(None)),
            raw => knot_types::BranchName::new(raw)
                .map(|name| Self(Some(name)))
                .map_err(de::Error::custom),
        }
    }
}

#[derive(Default)]
pub(crate) struct TagArg(Option<knot_types::TagName>);

impl TagArg {
    pub(crate) fn get(&self) -> Option<&knot_types::TagName> {
        self.0.as_ref()
    }
}

impl<'de> serde::Deserialize<'de> for TagArg {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)? {
            raw if raw.is_empty() => Ok(Self(None)),
            raw => {
                let short = raw.strip_prefix("refs/tags/").unwrap_or(&raw);
                knot_types::TagName::new(short)
                    .map(|name| Self(Some(name)))
                    .map_err(de::Error::custom)
            }
        }
    }
}

#[derive(Default)]
pub(crate) enum TreePath {
    #[default]
    Root,
    At(RepoPath),
    Outside(String),
}

impl TreePath {
    pub(crate) fn as_str(&self) -> &str {
        match self {
            TreePath::Root => "",
            TreePath::At(path) => path.as_str(),
            TreePath::Outside(raw) => raw,
        }
    }

    pub(crate) fn dir(&self) -> Option<Option<&RepoPath>> {
        match self {
            TreePath::Root => Some(None),
            TreePath::At(path) => Some(Some(path)),
            TreePath::Outside(_) => None,
        }
    }

    pub(crate) fn file(&self) -> Option<&RepoPath> {
        match self {
            TreePath::At(path) => Some(path),
            TreePath::Root | TreePath::Outside(_) => None,
        }
    }
}

impl<'de> Deserialize<'de> for TreePath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(match raw.is_empty() {
            true => TreePath::Root,
            false => match RepoPath::new(&raw) {
                Ok(path) => TreePath::At(path),
                Err(_) => TreePath::Outside(raw),
            },
        })
    }
}

#[derive(Default)]
pub(crate) struct RawFlag(bool);

impl RawFlag {
    pub(crate) fn requested(&self) -> bool {
        self.0
    }
}

impl<'de> Deserialize<'de> for RawFlag {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(RawFlag(String::deserialize(deserializer)? == "true"))
    }
}
