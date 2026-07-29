use serde::Deserialize;
use serde::de::{self, Deserializer};

use knot_types::{AtUri, RefName};
use url::Url;

pub(crate) struct RepoAtUri(AtUri<String>);

impl RepoAtUri {
    pub(crate) fn at_uri(&self) -> &AtUri<String> {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RepoAtUri {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        AtUri::new_owned(raw)
            .map(RepoAtUri)
            .map_err(|_| de::Error::custom("repo must be an at-uri"))
    }
}

#[derive(Clone)]
pub(crate) struct SourceUrl(Url);

impl SourceUrl {
    pub(crate) fn from_url(url: Url) -> Result<Self, &'static str> {
        match matches!(url.scheme(), "http" | "https") && url.has_host() {
            true => Ok(Self(url)),
            false => Err("source must be an http or https url"),
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub(crate) fn as_url(&self) -> &Url {
        &self.0
    }
}

impl<'de> Deserialize<'de> for SourceUrl {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        parse_source_url(&raw).map_err(de::Error::custom)
    }
}

fn parse_source_url(raw: &str) -> Result<SourceUrl, &'static str> {
    let url = Url::parse(raw).map_err(|_| "source must be a valid url")?;
    SourceUrl::from_url(url)
}

// A sourceless repo is a plain repo not a bad request necessarily,
// so empty string / missing field both have to make `None`.
pub(crate) fn optional_source_url<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<SourceUrl>, D::Error> {
    match Option::<String>::deserialize(deserializer)?.as_deref() {
        None | Some("") => Ok(None),
        Some(raw) => parse_source_url(raw).map(Some).map_err(de::Error::custom),
    }
}

knot_types::text_newtype! {
    pub(crate) struct Patch(String) => verbatim;
    pub(crate) struct CommitMessage(String) => verbatim;
    pub(crate) struct CommitBody(String) => verbatim;
}

pub(crate) use crate::query::Revspec;

pub(crate) struct ForkRef(RefName);

impl ForkRef {
    pub(crate) fn hidden_ref(&self, remote: &RemoteRef) -> Option<RefName> {
        RefName::new(format!("{}/{}", self.0.as_str(), remote.as_str())).ok()
    }
}

// Comes as a bare name,
// and `refs/hidden/` is the only place a fork staging ref is allowed to exist,
// so `Deserialize` prepends the prefix & `RefName::new` validates the whole string,
// therefore there's nothing to append after check.
impl<'de> Deserialize<'de> for ForkRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        RefName::new(format!("refs/hidden/{raw}"))
            .map(ForkRef)
            .map_err(|_| de::Error::custom("invalid fork ref"))
    }
}

pub(crate) struct RemoteRef(knot_types::BranchName);

impl RemoteRef {
    pub(crate) fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub(crate) fn head_ref(&self) -> RefName {
        self.0.head_ref()
    }
}

impl<'de> Deserialize<'de> for RemoteRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        knot_types::BranchName::new(raw)
            .map(RemoteRef)
            .map_err(|_| de::Error::custom("invalid remote ref"))
    }
}
