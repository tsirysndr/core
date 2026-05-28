use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use jacquard_common::deps::smol_str::SmolStr;
use jacquard_common::types::blob::BlobRef;
use jacquard_common::types::nsid::Nsid;
use jacquard_common::types::string::{AtUri, Datetime, Did};
use jacquard_common::types::value::Data;
use jacquard_common::{BosStr, DefaultStr};
use serde::{Deserialize, Deserializer};

use crate::edges::ExtractError;
use crate::sh_tangled::repo::pull::Round as CanonRound;

pub const LEGACY_COMMENT_SENTINEL_CID: &str = "bafkqaaa";

fn empty_string_as_none<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    let value: Option<serde_json::Value> = Deserialize::deserialize(d)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) if s.is_empty() => Ok(None),
        Some(other) => T::deserialize(other)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

#[derive(Debug, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename = "sh.tangled.repo.issue",
    tag = "$type",
    bound(deserialize = "S: Deserialize<'de> + BosStr")
)]
pub struct LegacyIssue<S: BosStr = DefaultStr> {
    pub created_at: Datetime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<S>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mentions: Option<Vec<Did<S>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub references: Option<Vec<AtUri<S>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<AtUri<S>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_did: Option<Did<S>>,
    pub title: S,
    #[serde(flatten, default, skip_serializing_if = "Option::is_none")]
    pub extra_data: Option<BTreeMap<SmolStr, Data<S>>>,
}

#[derive(Debug, Deserialize)]
#[serde(
    rename_all = "camelCase",
    bound(deserialize = "S: Deserialize<'de> + BosStr")
)]
pub struct LegacyTarget<S: BosStr = DefaultStr> {
    pub branch: S,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<AtUri<S>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "empty_string_as_none"
    )]
    pub repo_did: Option<Did<S>>,
}

#[derive(Debug, Deserialize)]
#[serde(
    rename_all = "camelCase",
    bound(deserialize = "S: Deserialize<'de> + BosStr")
)]
pub struct LegacySource<S: BosStr = DefaultStr> {
    pub branch: S,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<AtUri<S>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "empty_string_as_none"
    )]
    pub repo_did: Option<Did<S>>,
}

#[derive(Debug, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename = "sh.tangled.repo.pull",
    tag = "$type",
    bound(deserialize = "S: Deserialize<'de> + BosStr")
)]
pub struct LegacyPull<S: BosStr = DefaultStr> {
    pub created_at: Datetime,
    pub title: S,
    #[serde(default)]
    pub rounds: Vec<CanonRound<S>>,
    pub target: LegacyTarget<S>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<LegacySource<S>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<S>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mentions: Option<Vec<Did<S>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub references: Option<Vec<AtUri<S>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependent_on: Option<AtUri<S>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_blob: Option<BlobRef<S>>,
    #[serde(flatten, default, skip_serializing_if = "Option::is_none")]
    pub extra_data: Option<BTreeMap<SmolStr, Data<S>>>,
}

#[derive(Debug, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename = "sh.tangled.repo.issue.comment",
    tag = "$type",
    bound(deserialize = "S: Deserialize<'de> + BosStr")
)]
pub struct LegacyIssueComment<S: BosStr = DefaultStr> {
    pub created_at: Datetime,
    pub body: S,
    pub issue: AtUri<S>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<AtUri<S>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mentions: Option<Vec<Did<S>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub references: Option<Vec<AtUri<S>>>,
    #[serde(flatten, default, skip_serializing_if = "Option::is_none")]
    pub extra_data: Option<BTreeMap<SmolStr, Data<S>>>,
}

#[derive(Debug, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename = "sh.tangled.repo.pull.comment",
    tag = "$type",
    bound(deserialize = "S: Deserialize<'de> + BosStr")
)]
pub struct LegacyPullComment<S: BosStr = DefaultStr> {
    pub created_at: Datetime,
    pub body: S,
    pub pull: AtUri<S>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mentions: Option<Vec<Did<S>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub references: Option<Vec<AtUri<S>>>,
    #[serde(flatten, default, skip_serializing_if = "Option::is_none")]
    pub extra_data: Option<BTreeMap<SmolStr, Data<S>>>,
}

#[derive(Debug, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename = "sh.tangled.repo.collaborator",
    tag = "$type",
    bound(deserialize = "S: Deserialize<'de> + BosStr")
)]
pub struct LegacyCollaborator<S: BosStr = DefaultStr> {
    pub created_at: Datetime,
    pub subject: Did<S>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<AtUri<S>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_did: Option<Did<S>>,
    #[serde(flatten, default, skip_serializing_if = "Option::is_none")]
    pub extra_data: Option<BTreeMap<SmolStr, Data<S>>>,
}

#[derive(Debug, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename = "sh.tangled.git.refUpdate",
    tag = "$type",
    bound(deserialize = "S: Deserialize<'de> + BosStr")
)]
pub struct LegacyRefUpdate<S: BosStr = DefaultStr> {
    pub committer_did: Did<S>,
    pub meta: crate::sh_tangled::git::ref_update::Meta<S>,
    pub new_sha: S,
    pub old_sha: S,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_did: Option<Did<S>>,
    pub r#ref: S,
    pub repo_did: Did<S>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_name: Option<S>,
    #[serde(flatten, default, skip_serializing_if = "Option::is_none")]
    pub extra_data: Option<BTreeMap<SmolStr, Data<S>>>,
}

#[derive(Debug, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename = "sh.tangled.feed.star",
    tag = "$type",
    bound(deserialize = "S: Deserialize<'de> + BosStr")
)]
pub struct LegacyStar<S: BosStr = DefaultStr> {
    pub created_at: Datetime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<AtUri<S>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_did: Option<Did<S>>,
    #[serde(flatten, default, skip_serializing_if = "Option::is_none")]
    pub extra_data: Option<BTreeMap<SmolStr, Data<S>>>,
}

#[derive(Debug, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename = "sh.tangled.publicKey",
    tag = "$type",
    bound(deserialize = "S: Deserialize<'de> + BosStr")
)]
pub struct LegacyPublicKey<S: BosStr = DefaultStr> {
    pub created: Datetime,
    pub key: S,
    pub name: S,
    #[serde(flatten, default, skip_serializing_if = "Option::is_none")]
    pub extra_data: Option<BTreeMap<SmolStr, Data<S>>>,
}

#[derive(Debug, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename = "sh.tangled.repo",
    tag = "$type",
    bound(deserialize = "S: Deserialize<'de> + BosStr")
)]
pub struct LegacyRepo<S: BosStr = DefaultStr> {
    pub added_at: Datetime,
    pub knot: S,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<S>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<S>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<Did<S>>,
    #[serde(flatten, default, skip_serializing_if = "Option::is_none")]
    pub extra_data: Option<BTreeMap<SmolStr, Data<S>>>,
}

#[derive(Debug, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename = "sh.tangled.knot.member",
    tag = "$type",
    bound(deserialize = "S: Deserialize<'de> + BosStr")
)]
pub struct LegacyKnotMember<S: BosStr = DefaultStr> {
    pub added_at: Datetime,
    pub domain: S,
    pub member: Did<S>,
    #[serde(flatten, default, skip_serializing_if = "Option::is_none")]
    pub extra_data: Option<BTreeMap<SmolStr, Data<S>>>,
}

#[derive(Debug)]
pub enum LegacyRecord {
    Issue(LegacyIssue<DefaultStr>),
    IssueComment(LegacyIssueComment<DefaultStr>),
    Pull(LegacyPull<DefaultStr>),
    PullComment(LegacyPullComment<DefaultStr>),
    Collaborator(LegacyCollaborator<DefaultStr>),
    RefUpdate(LegacyRefUpdate<DefaultStr>),
    Star(LegacyStar<DefaultStr>),
    PublicKey(LegacyPublicKey<DefaultStr>),
    Repo(LegacyRepo<DefaultStr>),
    KnotMember(LegacyKnotMember<DefaultStr>),
}

impl LegacyRecord {
    pub fn from_json_bytes<S: BosStr + AsRef<str>>(
        nsid: &Nsid<S>,
        bytes: &[u8],
    ) -> Result<Self, ExtractError> {
        match nsid.as_ref() {
            "sh.tangled.repo.issue" => Ok(Self::Issue(serde_json::from_slice(bytes)?)),
            "sh.tangled.repo.issue.comment" => {
                Ok(Self::IssueComment(serde_json::from_slice(bytes)?))
            }
            "sh.tangled.repo.pull" => Ok(Self::Pull(serde_json::from_slice(bytes)?)),
            "sh.tangled.repo.pull.comment" => Ok(Self::PullComment(serde_json::from_slice(bytes)?)),
            "sh.tangled.repo.collaborator" => {
                Ok(Self::Collaborator(serde_json::from_slice(bytes)?))
            }
            "sh.tangled.git.refUpdate" => Ok(Self::RefUpdate(serde_json::from_slice(bytes)?)),
            "sh.tangled.feed.star" => Ok(Self::Star(serde_json::from_slice(bytes)?)),
            "sh.tangled.publicKey" => Ok(Self::PublicKey(serde_json::from_slice(bytes)?)),
            "sh.tangled.repo" => Ok(Self::Repo(serde_json::from_slice(bytes)?)),
            "sh.tangled.knot.member" => Ok(Self::KnotMember(serde_json::from_slice(bytes)?)),
            other => Err(ExtractError::UnknownCollection(other.into())),
        }
    }
}
