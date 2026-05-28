use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;

use jacquard_common::types::ident::AtIdentifier;
use jacquard_common::types::nsid::Nsid;
use jacquard_common::types::string::{AtUri, Did};
use jacquard_common::{BosStr, DefaultStr, IntoStatic};
use serde::Serialize;

use crate::edges::{ExtractError, Record};
use crate::ids::nsid_static;
use crate::sh_tangled::actor::profile::Profile;
use crate::sh_tangled::feed::comment::Comment as FeedCommentRecord;
use crate::sh_tangled::label::definition::Definition as LabelDefinitionRecord;
use crate::sh_tangled::repo::Repo as RepoRecord;
use crate::sh_tangled::repo::issue::Issue;
use crate::sh_tangled::repo::pull::Pull;
use crate::sh_tangled::string::TangledString;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchDoc {
    pub uri: AtUri<DefaultStr>,
    pub nsid: Nsid<DefaultStr>,
    pub title: String,
    pub body: String,
    pub author: Option<Did<DefaultStr>>,
    pub created_at: Option<i64>,
    pub repo: Option<Did<DefaultStr>>,
}

pub trait SearchSink: Send + Sync {
    fn upsert(&self, doc: SearchDoc) -> impl Future<Output = ()> + Send;
    fn remove(&self, uri: &AtUri<DefaultStr>) -> impl Future<Output = ()> + Send;
}

pub struct NoopSearchSink;

impl SearchSink for NoopSearchSink {
    async fn upsert(&self, _doc: SearchDoc) {}
    async fn remove(&self, _uri: &AtUri<DefaultStr>) {}
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum SearchableRecord {
    Profile(Profile<DefaultStr>),
    Repo(RepoRecord<DefaultStr>),
    Issue(Issue<DefaultStr>),
    Pull(Pull<DefaultStr>),
    FeedComment(FeedCommentRecord<DefaultStr>),
    TangledString(TangledString<DefaultStr>),
    LabelDefinition(LabelDefinitionRecord<DefaultStr>),
}

impl SearchableRecord {
    pub fn try_from_record(record: Record) -> Option<Self> {
        match record {
            Record::Profile(r) => Some(Self::Profile(r)),
            Record::Repo(r) => Some(Self::Repo(r)),
            Record::Issue(r) => Some(Self::Issue(r)),
            Record::Pull(r) => Some(Self::Pull(r)),
            Record::FeedComment(r) => Some(Self::FeedComment(r)),
            Record::TangledString(r) => Some(Self::TangledString(r)),
            Record::LabelDefinition(r) => Some(Self::LabelDefinition(r)),
            Record::Reaction(_)
            | Record::Star(_)
            | Record::RefUpdate(_)
            | Record::Follow(_)
            | Record::Vouch(_)
            | Record::Knot(_)
            | Record::KnotMember(_)
            | Record::LabelOp(_)
            | Record::Pipeline(_)
            | Record::PipelineStatus(_)
            | Record::PublicKey(_)
            | Record::Artifact(_)
            | Record::Collaborator(_)
            | Record::IssueState(_)
            | Record::PullStatus(_)
            | Record::Spindle(_)
            | Record::SpindleMember(_) => None,
        }
    }

    pub fn from_json_bytes<S: BosStr + AsRef<str>>(
        nsid: &Nsid<S>,
        bytes: &[u8],
    ) -> Result<Self, ExtractError> {
        let parsed = Record::from_json_bytes(nsid, bytes)?;
        Self::try_from_record(parsed)
            .ok_or_else(|| ExtractError::UnknownCollection(nsid.as_ref().into()))
    }

    pub fn nsid(&self) -> Nsid<DefaultStr> {
        let s: &'static str = match self {
            Self::Profile(_) => "sh.tangled.actor.profile",
            Self::Repo(_) => "sh.tangled.repo",
            Self::Issue(_) => "sh.tangled.repo.issue",
            Self::Pull(_) => "sh.tangled.repo.pull",
            Self::FeedComment(_) => "sh.tangled.feed.comment",
            Self::TangledString(_) => "sh.tangled.string",
            Self::LabelDefinition(_) => "sh.tangled.label.definition",
        };
        nsid_static(s)
    }

    pub fn to_search_doc(&self, source: &AtUri<DefaultStr>) -> SearchDoc {
        match self {
            Self::Profile(r) => profile_doc(source, r),
            Self::Repo(r) => repo_doc(source, r),
            Self::Issue(r) => issue_doc(source, r),
            Self::Pull(r) => pull_doc(source, r),
            Self::FeedComment(r) => feed_comment_doc(source, r),
            Self::TangledString(r) => string_doc(source, r),
            Self::LabelDefinition(r) => label_definition_doc(source, r),
        }
    }
}

fn author_of(source: &AtUri<DefaultStr>) -> Option<Did<DefaultStr>> {
    match source.authority() {
        AtIdentifier::Did(d) => Some(d.into_static()),
        AtIdentifier::Handle(_) => None,
    }
}

fn doc(
    source: &AtUri<DefaultStr>,
    nsid: &'static str,
    title: &str,
    body_parts: Vec<String>,
    created_at: Option<i64>,
    repo: Option<Did<DefaultStr>>,
) -> SearchDoc {
    SearchDoc {
        uri: source.clone(),
        nsid: nsid_static(nsid),
        title: title.to_owned(),
        body: body_parts.join(" "),
        author: author_of(source),
        created_at,
        repo,
    }
}

fn profile_doc(source: &AtUri<DefaultStr>, r: &Profile<DefaultStr>) -> SearchDoc {
    let title = r
        .preferred_handle
        .as_ref()
        .map(|h| h.as_str().to_owned())
        .unwrap_or_default();
    let mut parts: Vec<String> = Vec::new();
    if let Some(d) = &r.description {
        parts.push(d.as_str().to_owned());
    }
    if let Some(loc) = &r.location {
        parts.push(loc.as_str().to_owned());
    }
    if let Some(p) = &r.pronouns {
        parts.push(p.as_str().to_owned());
    }
    doc(
        source,
        "sh.tangled.actor.profile",
        &title,
        parts,
        None,
        None,
    )
}

fn repo_doc(source: &AtUri<DefaultStr>, r: &RepoRecord<DefaultStr>) -> SearchDoc {
    let mut parts: Vec<String> = Vec::new();
    if let Some(d) = &r.description {
        parts.push(d.as_str().to_owned());
    }
    if let Some(topics) = &r.topics {
        parts.extend(topics.iter().map(|t| t.as_str().to_owned()));
    }
    let rkey = source.rkey();
    let title = r
        .name
        .as_deref()
        .or_else(|| rkey.as_ref().map(|k| k.as_str()))
        .unwrap_or("");
    doc(
        source,
        "sh.tangled.repo",
        title,
        parts,
        Some(r.created_at.timestamp()),
        r.repo_did.clone(),
    )
}

fn issue_doc(source: &AtUri<DefaultStr>, r: &Issue<DefaultStr>) -> SearchDoc {
    let body = r
        .body
        .as_ref()
        .map(|b| Vec::from([b.as_str().to_owned()]))
        .unwrap_or_default();
    doc(
        source,
        "sh.tangled.repo.issue",
        r.title.as_str(),
        body,
        Some(r.created_at.timestamp()),
        Some(r.repo.clone()),
    )
}

fn feed_comment_doc(source: &AtUri<DefaultStr>, r: &FeedCommentRecord<DefaultStr>) -> SearchDoc {
    doc(
        source,
        "sh.tangled.feed.comment",
        "",
        Vec::from([r.body.text.as_str().to_owned()]),
        Some(r.created_at.timestamp()),
        None,
    )
}

fn pull_doc(source: &AtUri<DefaultStr>, r: &Pull<DefaultStr>) -> SearchDoc {
    let body = r
        .body
        .as_ref()
        .map(|b| Vec::from([b.as_str().to_owned()]))
        .unwrap_or_default();
    doc(
        source,
        "sh.tangled.repo.pull",
        r.title.as_str(),
        body,
        Some(r.created_at.timestamp()),
        Some(r.target.repo.clone()),
    )
}

fn string_doc(source: &AtUri<DefaultStr>, r: &TangledString<DefaultStr>) -> SearchDoc {
    doc(
        source,
        "sh.tangled.string",
        r.filename.as_str(),
        Vec::from([
            r.description.as_str().to_owned(),
            r.contents.as_str().to_owned(),
        ]),
        Some(r.created_at.timestamp()),
        None,
    )
}

fn label_definition_doc(
    source: &AtUri<DefaultStr>,
    r: &LabelDefinitionRecord<DefaultStr>,
) -> SearchDoc {
    doc(
        source,
        "sh.tangled.label.definition",
        r.name.as_str(),
        Vec::new(),
        Some(r.created_at.timestamp()),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use jacquard_common::types::nsid::Nsid;
    use serde_json::json;

    fn at(s: &str) -> AtUri<DefaultStr> {
        AtUri::new_owned(s).unwrap()
    }

    fn nsid(s: &'static str) -> Nsid<DefaultStr> {
        Nsid::new_static(s).unwrap()
    }

    fn extract(
        collection: &Nsid<DefaultStr>,
        source: &AtUri<DefaultStr>,
        body: serde_json::Value,
    ) -> SearchDoc {
        let parsed = Record::from_json_value(collection, body).expect("parse");
        let searchable = SearchableRecord::try_from_record(parsed).expect("indexable record");
        searchable.to_search_doc(source)
    }

    #[test]
    fn issue_titles_and_body_indexable() {
        let doc = extract(
            &nsid("sh.tangled.repo.issue"),
            &at("at://did:plc:nel/sh.tangled.repo.issue/abcabcabcabcz"),
            json!({
                "$type": "sh.tangled.repo.issue",
                "repo": "did:plc:squid",
                "title": "barnacle pagination overflow",
                "body": "scrolling resets when the cursor wraps",
                "createdAt": "2026-05-01T00:00:00Z"
            }),
        );
        assert_eq!(doc.title, "barnacle pagination overflow");
        assert_eq!(doc.body, "scrolling resets when the cursor wraps");
        assert_eq!(doc.nsid, nsid("sh.tangled.repo.issue"));
    }

    #[test]
    fn issue_without_body_yields_empty_body() {
        let doc = extract(
            &nsid("sh.tangled.repo.issue"),
            &at("at://did:plc:nel/sh.tangled.repo.issue/abcabcabcabcz"),
            json!({
                "$type": "sh.tangled.repo.issue",
                "repo": "did:plc:squid",
                "title": "kelp ate my newline",
                "createdAt": "2026-05-01T00:00:00Z"
            }),
        );
        assert_eq!(doc.title, "kelp ate my newline");
        assert!(doc.body.is_empty());
    }

    #[test]
    fn repo_indexes_name_description_topics() {
        let doc = extract(
            &nsid("sh.tangled.repo"),
            &at("at://did:plc:teq/sh.tangled.repo/r1"),
            json!({
                "$type": "sh.tangled.repo",
                "name": "scallop",
                "knot": "oyster.cafe",
                "description": "shell index for tide pools",
                "topics": ["intertidal", "molluscs"],
                "createdAt": "2026-05-01T00:00:00Z"
            }),
        );
        assert_eq!(doc.title, "scallop");
        assert!(doc.body.contains("shell index"));
        assert!(doc.body.contains("intertidal"));
        assert!(doc.body.contains("molluscs"));
    }

    #[test]
    fn repo_without_name_field_falls_back_to_rkey() {
        let doc = extract(
            &nsid("sh.tangled.repo"),
            &at("at://did:plc:teq/sh.tangled.repo/limpet"),
            json!({
                "$type": "sh.tangled.repo",
                "knot": "oyster.cafe",
                "createdAt": "2026-05-01T00:00:00Z"
            }),
        );
        assert_eq!(doc.title, "limpet");
    }

    #[test]
    fn non_text_records_have_no_searchable_view() {
        let parsed = Record::from_json_value(
            &nsid("sh.tangled.feed.star"),
            json!({
                "$type": "sh.tangled.feed.star",
                "createdAt": "2026-05-01T00:00:00Z",
                "subject": {
                    "$type": "sh.tangled.feed.star#repo",
                    "did": "did:plc:squid"
                }
            }),
        )
        .expect("parse");
        assert!(SearchableRecord::try_from_record(parsed).is_none());
    }

    #[test]
    fn string_indexes_filename_description_contents() {
        let doc = extract(
            &nsid("sh.tangled.string"),
            &at("at://did:plc:teq/sh.tangled.string/k1"),
            json!({
                "$type": "sh.tangled.string",
                "filename": "anemone.md",
                "description": "field notes on tide pools",
                "contents": "the limpet returned at dawn",
                "createdAt": "2026-05-01T00:00:00Z"
            }),
        );
        assert_eq!(doc.title, "anemone.md");
        assert!(doc.body.contains("field notes"));
        assert!(doc.body.contains("limpet returned at dawn"));
    }

    #[test]
    fn from_json_bytes_round_trips_via_serialize() {
        let body = json!({
            "$type": "sh.tangled.repo.issue",
            "repo": "did:plc:squid",
            "title": "uni shell",
            "createdAt": "2026-05-01T00:00:00Z"
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let view =
            SearchableRecord::from_json_bytes(&nsid("sh.tangled.repo.issue"), &bytes).unwrap();
        assert_eq!(view.nsid(), nsid("sh.tangled.repo.issue"));
        let serialized = serde_json::to_value(&view).unwrap();
        assert_eq!(serialized["title"], json!("uni shell"));
        assert_eq!(serialized["$type"], json!("sh.tangled.repo.issue"));
    }

    #[test]
    fn from_json_bytes_rejects_unknown_nsid() {
        let body = json!({
            "$type": "sh.tangled.feed.star",
            "createdAt": "2026-05-01T00:00:00Z",
            "subject": {
                "$type": "sh.tangled.feed.star#repo",
                "did": "did:plc:squid"
            }
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let err = SearchableRecord::from_json_bytes(&nsid("sh.tangled.feed.star"), &bytes)
            .expect_err("non-searchable nsid must be rejected at search hydration");
        assert!(matches!(err, ExtractError::UnknownCollection(_)));
    }
}
