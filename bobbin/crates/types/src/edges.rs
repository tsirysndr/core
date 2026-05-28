use alloc::vec;
use alloc::vec::Vec;

use jacquard_common::types::did::Did;
use jacquard_common::types::nsid::Nsid;
use jacquard_common::types::string::{AtStrError, AtUri, Datetime};
use jacquard_common::types::tid::Tid;
use jacquard_common::{BosStr, DefaultStr};

use crate::ids::{SubjectRef, nsid_static};
use crate::sh_tangled::actor::profile::Profile;
use crate::sh_tangled::feed::reaction::Reaction;
use crate::sh_tangled::feed::star::Star;
use crate::sh_tangled::git::ref_update::RefUpdate;
use crate::sh_tangled::graph::follow::Follow;
use crate::sh_tangled::graph::vouch::Vouch;
use crate::sh_tangled::knot::Knot;
use crate::sh_tangled::knot::member::Member as KnotMemberRecord;
use crate::sh_tangled::label::definition::Definition as LabelDefinitionRecord;
use crate::sh_tangled::label::op::Op as LabelOpRecord;
use crate::sh_tangled::pipeline::Pipeline;
use crate::sh_tangled::pipeline::status::Status as PipelineStatusRecord;
use crate::sh_tangled::public_key::PublicKey;
use crate::sh_tangled::repo::Repo as RepoRecord;
use crate::sh_tangled::repo::artifact::Artifact;
use crate::sh_tangled::repo::collaborator::Collaborator;
use crate::sh_tangled::repo::issue::Issue;
use crate::sh_tangled::repo::issue::comment::Comment as IssueCommentRecord;
use crate::sh_tangled::repo::issue::state::State as IssueStateRecord;
use crate::sh_tangled::repo::pull::Pull;
use crate::sh_tangled::repo::pull::comment::Comment as PullCommentRecord;
use crate::sh_tangled::repo::pull::status::Status as PullStatusRecord;
use crate::sh_tangled::spindle::Spindle;
use crate::sh_tangled::spindle::member::Member as SpindleMemberRecord;
use crate::sh_tangled::string::TangledString;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Edge {
    pub kind: Nsid<DefaultStr>,
    pub subject: SubjectRef,
    pub source: AtUri<DefaultStr>,
    pub sort_micros: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("decode JSON record body: {0}")]
    DecodeJson(#[from] serde_json::Error),
    #[error("invalid AT-URI synthesized from record subject: {0}")]
    InvalidAtUri(#[from] AtStrError),
    #[error("unknown collection NSID: {0}")]
    UnknownCollection(alloc::string::String),
}

#[derive(Debug)]
pub enum Record {
    Profile(Profile<DefaultStr>),
    Reaction(Reaction<DefaultStr>),
    Star(Star<DefaultStr>),
    RefUpdate(RefUpdate<DefaultStr>),
    Follow(Follow<DefaultStr>),
    Vouch(Vouch<DefaultStr>),
    Knot(Knot<DefaultStr>),
    KnotMember(KnotMemberRecord<DefaultStr>),
    LabelDefinition(LabelDefinitionRecord<DefaultStr>),
    LabelOp(LabelOpRecord<DefaultStr>),
    Pipeline(Pipeline<DefaultStr>),
    PipelineStatus(PipelineStatusRecord<DefaultStr>),
    PublicKey(PublicKey<DefaultStr>),
    Repo(RepoRecord<DefaultStr>),
    Artifact(Artifact<DefaultStr>),
    Collaborator(Collaborator<DefaultStr>),
    Issue(Issue<DefaultStr>),
    IssueComment(IssueCommentRecord<DefaultStr>),
    IssueState(IssueStateRecord<DefaultStr>),
    Pull(Pull<DefaultStr>),
    PullComment(PullCommentRecord<DefaultStr>),
    PullStatus(PullStatusRecord<DefaultStr>),
    Spindle(Spindle<DefaultStr>),
    SpindleMember(SpindleMemberRecord<DefaultStr>),
    TangledString(TangledString<DefaultStr>),
}

impl Record {
    pub fn from_json_value<S: BosStr + AsRef<str>>(
        collection: &Nsid<S>,
        value: serde_json::Value,
    ) -> Result<Self, ExtractError> {
        let bytes = serde_json::to_vec(&value)?;
        Self::from_json_bytes(collection, &bytes)
    }

    pub fn from_json_bytes<S: BosStr + AsRef<str>>(
        nsid: &Nsid<S>,
        bytes: &[u8],
    ) -> Result<Self, ExtractError> {
        macro_rules! parse {
            ($variant:ident) => {
                Ok(Self::$variant(serde_json::from_slice(bytes)?))
            };
        }
        match nsid.as_ref() {
            "sh.tangled.actor.profile" => parse!(Profile),
            "sh.tangled.feed.reaction" => parse!(Reaction),
            "sh.tangled.feed.star" => parse!(Star),
            "sh.tangled.git.refUpdate" => parse!(RefUpdate),
            "sh.tangled.graph.follow" => parse!(Follow),
            "sh.tangled.graph.vouch" => parse!(Vouch),
            "sh.tangled.knot" => parse!(Knot),
            "sh.tangled.knot.member" => parse!(KnotMember),
            "sh.tangled.label.definition" => parse!(LabelDefinition),
            "sh.tangled.label.op" => parse!(LabelOp),
            "sh.tangled.pipeline" => parse!(Pipeline),
            "sh.tangled.pipeline.status" => parse!(PipelineStatus),
            "sh.tangled.publicKey" => parse!(PublicKey),
            "sh.tangled.repo" => parse!(Repo),
            "sh.tangled.repo.artifact" => parse!(Artifact),
            "sh.tangled.repo.collaborator" => parse!(Collaborator),
            "sh.tangled.repo.issue" => parse!(Issue),
            "sh.tangled.repo.issue.comment" => parse!(IssueComment),
            "sh.tangled.repo.issue.state" => parse!(IssueState),
            "sh.tangled.repo.pull" => parse!(Pull),
            "sh.tangled.repo.pull.comment" => parse!(PullComment),
            "sh.tangled.repo.pull.status" => parse!(PullStatus),
            "sh.tangled.spindle" => parse!(Spindle),
            "sh.tangled.spindle.member" => parse!(SpindleMember),
            "sh.tangled.string" => parse!(TangledString),
            other => Err(ExtractError::UnknownCollection(other.into())),
        }
    }

    pub fn collection(&self) -> Nsid<DefaultStr> {
        let s: &'static str = match self {
            Self::Profile(_) => "sh.tangled.actor.profile",
            Self::Reaction(_) => "sh.tangled.feed.reaction",
            Self::Star(_) => "sh.tangled.feed.star",
            Self::RefUpdate(_) => "sh.tangled.git.refUpdate",
            Self::Follow(_) => "sh.tangled.graph.follow",
            Self::Vouch(_) => "sh.tangled.graph.vouch",
            Self::Knot(_) => "sh.tangled.knot",
            Self::KnotMember(_) => "sh.tangled.knot.member",
            Self::LabelDefinition(_) => "sh.tangled.label.definition",
            Self::LabelOp(_) => "sh.tangled.label.op",
            Self::Pipeline(_) => "sh.tangled.pipeline",
            Self::PipelineStatus(_) => "sh.tangled.pipeline.status",
            Self::PublicKey(_) => "sh.tangled.publicKey",
            Self::Repo(_) => "sh.tangled.repo",
            Self::Artifact(_) => "sh.tangled.repo.artifact",
            Self::Collaborator(_) => "sh.tangled.repo.collaborator",
            Self::Issue(_) => "sh.tangled.repo.issue",
            Self::IssueComment(_) => "sh.tangled.repo.issue.comment",
            Self::IssueState(_) => "sh.tangled.repo.issue.state",
            Self::Pull(_) => "sh.tangled.repo.pull",
            Self::PullComment(_) => "sh.tangled.repo.pull.comment",
            Self::PullStatus(_) => "sh.tangled.repo.pull.status",
            Self::Spindle(_) => "sh.tangled.spindle",
            Self::SpindleMember(_) => "sh.tangled.spindle.member",
            Self::TangledString(_) => "sh.tangled.string",
        };
        nsid_static(s)
    }

    pub fn extract_edges(&self, source: &AtUri<DefaultStr>) -> Result<Vec<Edge>, ExtractError> {
        let sort_micros = self.sort_micros_for(source);
        let mut primary = self.primary_edges(source)?;
        primary.iter_mut().for_each(|e| e.sort_micros = sort_micros);
        Ok(append_mirror_edges(primary, source))
    }

    pub fn sort_micros_for(&self, source: &AtUri<DefaultStr>) -> u64 {
        if let Some(dt) = self.created_at() {
            let micros = dt.timestamp_micros();
            if micros >= 0 {
                return micros as u64;
            }
        }
        if let Some(rkey) = source.rkey()
            && let Ok(tid) = Tid::new(rkey.as_ref())
        {
            return tid.timestamp();
        }
        0
    }

    fn created_at(&self) -> Option<&Datetime> {
        match self {
            Self::Profile(_) => None,
            Self::Reaction(r) => Some(&r.created_at),
            Self::Star(r) => Some(&r.created_at),
            Self::RefUpdate(_) => None,
            Self::Follow(r) => Some(&r.created_at),
            Self::Vouch(r) => Some(&r.created_at),
            Self::Knot(r) => Some(&r.created_at),
            Self::KnotMember(r) => Some(&r.created_at),
            Self::LabelDefinition(r) => Some(&r.created_at),
            Self::LabelOp(r) => Some(&r.performed_at),
            Self::Pipeline(_) => None,
            Self::PipelineStatus(r) => Some(&r.created_at),
            Self::PublicKey(r) => Some(&r.created_at),
            Self::Repo(r) => Some(&r.created_at),
            Self::Artifact(r) => Some(&r.created_at),
            Self::Collaborator(r) => Some(&r.created_at),
            Self::Issue(r) => Some(&r.created_at),
            Self::IssueComment(r) => Some(&r.created_at),
            Self::IssueState(_) => None,
            Self::Pull(r) => Some(&r.created_at),
            Self::PullComment(r) => Some(&r.created_at),
            Self::PullStatus(_) => None,
            Self::Spindle(r) => Some(&r.created_at),
            Self::SpindleMember(r) => Some(&r.created_at),
            Self::TangledString(r) => Some(&r.created_at),
        }
    }

    fn primary_edges(&self, source: &AtUri<DefaultStr>) -> Result<Vec<Edge>, ExtractError> {
        match self {
            Self::Star(r) => star_edges(source, r),
            Self::Reaction(r) => reaction_edges(source, r),
            Self::Follow(r) => follow_edges(source, r),
            Self::RefUpdate(r) => ref_update_edges(source, r),
            Self::KnotMember(r) => knot_member_edges(source, r),
            Self::LabelOp(r) => label_op_edges(source, r),
            Self::PipelineStatus(r) => pipeline_status_edges(source, r),
            Self::Artifact(r) => artifact_edges(source, r),
            Self::Collaborator(r) => collaborator_edges(source, r),
            Self::Issue(r) => issue_edges(source, r),
            Self::IssueComment(r) => issue_comment_edges(source, r),
            Self::IssueState(r) => issue_state_edges(source, r),
            Self::Pull(r) => pull_edges(source, r),
            Self::PullComment(r) => pull_comment_edges(source, r),
            Self::PullStatus(r) => pull_status_edges(source, r),
            Self::SpindleMember(r) => spindle_member_edges(source, r),
            Self::Pipeline(r) => pipeline_edges(source, r),
            Self::Knot(_) => Ok(owner_self_edges("sh.tangled.knot", source)),
            Self::LabelDefinition(_) => Ok(owner_self_edges("sh.tangled.label.definition", source)),
            Self::PublicKey(_) => Ok(owner_self_edges("sh.tangled.publicKey", source)),
            Self::Repo(_) => Ok(owner_self_edges("sh.tangled.repo", source)),
            Self::Spindle(_) => Ok(owner_self_edges("sh.tangled.spindle", source)),
            Self::TangledString(_) => Ok(owner_self_edges("sh.tangled.string", source)),
            Self::Vouch(r) => vouch_edges(source, r),
            Self::Profile(_) => Ok(Vec::new()),
        }
    }
}

fn repo_subject(
    uri: &Option<AtUri<DefaultStr>>,
    did: &Option<Did<DefaultStr>>,
) -> Option<SubjectRef> {
    if let Some(d) = did.as_ref() {
        return Some(SubjectRef::Did(d.clone()));
    }
    uri_subject_for_record(uri.as_ref()?)
}

fn uri_subject_for_record(uri: &AtUri<DefaultStr>) -> Option<SubjectRef> {
    crate::ids::owner_did_from_aturi(uri)?;
    Some(SubjectRef::Uri(uri.clone()))
}

fn one_edge(kind: &'static str, subject: SubjectRef, source: &AtUri<DefaultStr>) -> Vec<Edge> {
    vec![Edge {
        kind: nsid_static(kind),
        subject,
        source: source.clone(),
        sort_micros: 0,
    }]
}

const MIRROR_KINDS: &[(&str, &str)] = &[
    ("sh.tangled.feed.star", "sh.tangled.feed.star.by"),
    ("sh.tangled.feed.reaction", "sh.tangled.feed.reaction.by"),
    ("sh.tangled.graph.follow", "sh.tangled.graph.follow.by"),
    ("sh.tangled.graph.vouch", "sh.tangled.graph.vouch.by"),
    ("sh.tangled.git.refUpdate", "sh.tangled.git.refUpdate.by"),
    ("sh.tangled.knot.member", "sh.tangled.knot.member.by"),
    ("sh.tangled.label.op", "sh.tangled.label.op.by"),
    ("sh.tangled.pipeline", "sh.tangled.pipeline.by"),
    (
        "sh.tangled.pipeline.status",
        "sh.tangled.pipeline.status.by",
    ),
    ("sh.tangled.repo.artifact", "sh.tangled.repo.artifact.by"),
    (
        "sh.tangled.repo.collaborator",
        "sh.tangled.repo.collaborator.by",
    ),
    ("sh.tangled.repo.issue", "sh.tangled.repo.issue.by"),
    (
        "sh.tangled.repo.issue.comment",
        "sh.tangled.repo.issue.comment.by",
    ),
    (
        "sh.tangled.repo.issue.state",
        "sh.tangled.repo.issue.state.by",
    ),
    ("sh.tangled.repo.pull", "sh.tangled.repo.pull.by"),
    (
        "sh.tangled.repo.pull.comment",
        "sh.tangled.repo.pull.comment.by",
    ),
    (
        "sh.tangled.repo.pull.status",
        "sh.tangled.repo.pull.status.by",
    ),
    ("sh.tangled.spindle.member", "sh.tangled.spindle.member.by"),
];

fn mirror_kind_for(kind: &str) -> Option<&'static str> {
    MIRROR_KINDS
        .iter()
        .find(|(primary, _)| *primary == kind)
        .map(|(_, mirror)| *mirror)
}

fn append_mirror_edges(primary: Vec<Edge>, source: &AtUri<DefaultStr>) -> Vec<Edge> {
    let Some(author) = crate::ids::owner_did_from_aturi(source) else {
        return primary;
    };
    let author_subject = SubjectRef::Did(author);
    let mirrors: Vec<Edge> = primary
        .iter()
        .filter_map(|edge| {
            let mirror_nsid = mirror_kind_for(edge.kind.as_ref())?;
            (edge.subject != author_subject).then(|| Edge {
                kind: nsid_static(mirror_nsid),
                subject: author_subject.clone(),
                source: edge.source.clone(),
                sort_micros: edge.sort_micros,
            })
        })
        .collect();
    primary.into_iter().chain(mirrors).collect()
}

fn star_edges(
    source: &AtUri<DefaultStr>,
    record: &Star<DefaultStr>,
) -> Result<Vec<Edge>, ExtractError> {
    use crate::sh_tangled::feed::star::StarSubject;
    let subject = match &record.subject {
        StarSubject::Repo(r) => SubjectRef::Did(r.did.clone()),
        StarSubject::String(s) => {
            let Some(uri_subject) = uri_subject_for_record(&s.uri) else {
                return Ok(Vec::new());
            };
            uri_subject
        }
    };
    Ok(one_edge("sh.tangled.feed.star", subject, source))
}

fn reaction_edges(
    source: &AtUri<DefaultStr>,
    record: &Reaction<DefaultStr>,
) -> Result<Vec<Edge>, ExtractError> {
    let Some(subject) = uri_subject_for_record(&record.subject) else {
        return Ok(Vec::new());
    };
    Ok(one_edge("sh.tangled.feed.reaction", subject, source))
}

fn follow_edges(
    source: &AtUri<DefaultStr>,
    record: &Follow<DefaultStr>,
) -> Result<Vec<Edge>, ExtractError> {
    Ok(one_edge(
        "sh.tangled.graph.follow",
        SubjectRef::Did(record.subject.clone()),
        source,
    ))
}

fn ref_update_edges(
    source: &AtUri<DefaultStr>,
    record: &RefUpdate<DefaultStr>,
) -> Result<Vec<Edge>, ExtractError> {
    Ok(one_edge(
        "sh.tangled.git.refUpdate",
        SubjectRef::Did(record.repo.clone()),
        source,
    ))
}

fn knot_member_edges(
    source: &AtUri<DefaultStr>,
    record: &KnotMemberRecord<DefaultStr>,
) -> Result<Vec<Edge>, ExtractError> {
    Ok(one_edge(
        "sh.tangled.knot.member",
        SubjectRef::Did(record.subject.clone()),
        source,
    ))
}

fn label_op_edges(
    source: &AtUri<DefaultStr>,
    record: &LabelOpRecord<DefaultStr>,
) -> Result<Vec<Edge>, ExtractError> {
    let Some(subject) = uri_subject_for_record(&record.subject) else {
        return Ok(Vec::new());
    };
    Ok(one_edge("sh.tangled.label.op", subject, source))
}

fn pipeline_status_edges(
    source: &AtUri<DefaultStr>,
    record: &PipelineStatusRecord<DefaultStr>,
) -> Result<Vec<Edge>, ExtractError> {
    let Some(subject) = uri_subject_for_record(&record.pipeline) else {
        return Ok(Vec::new());
    };
    Ok(one_edge("sh.tangled.pipeline.status", subject, source))
}

fn artifact_edges(
    source: &AtUri<DefaultStr>,
    record: &Artifact<DefaultStr>,
) -> Result<Vec<Edge>, ExtractError> {
    let Some(subject) = repo_subject(&record.repo, &record.repo_did) else {
        return Ok(Vec::new());
    };
    Ok(one_edge("sh.tangled.repo.artifact", subject, source))
}

fn collaborator_edges(
    source: &AtUri<DefaultStr>,
    record: &Collaborator<DefaultStr>,
) -> Result<Vec<Edge>, ExtractError> {
    Ok(one_edge(
        "sh.tangled.repo.collaborator",
        SubjectRef::Did(record.repo.clone()),
        source,
    ))
}

fn issue_edges(
    source: &AtUri<DefaultStr>,
    record: &Issue<DefaultStr>,
) -> Result<Vec<Edge>, ExtractError> {
    Ok(one_edge(
        "sh.tangled.repo.issue",
        SubjectRef::Did(record.repo.clone()),
        source,
    ))
}

fn issue_comment_edges(
    source: &AtUri<DefaultStr>,
    record: &IssueCommentRecord<DefaultStr>,
) -> Result<Vec<Edge>, ExtractError> {
    let Some(subject) = uri_subject_for_record(&record.issue) else {
        return Ok(Vec::new());
    };
    Ok(one_edge("sh.tangled.repo.issue.comment", subject, source))
}

fn issue_state_edges(
    source: &AtUri<DefaultStr>,
    record: &IssueStateRecord<DefaultStr>,
) -> Result<Vec<Edge>, ExtractError> {
    let Some(subject) = uri_subject_for_record(&record.issue) else {
        return Ok(Vec::new());
    };
    Ok(one_edge("sh.tangled.repo.issue.state", subject, source))
}

fn pull_edges(
    source: &AtUri<DefaultStr>,
    record: &Pull<DefaultStr>,
) -> Result<Vec<Edge>, ExtractError> {
    Ok(one_edge(
        "sh.tangled.repo.pull",
        SubjectRef::Did(record.target.repo.clone()),
        source,
    ))
}

fn pull_comment_edges(
    source: &AtUri<DefaultStr>,
    record: &PullCommentRecord<DefaultStr>,
) -> Result<Vec<Edge>, ExtractError> {
    let Some(subject) = uri_subject_for_record(&record.pull) else {
        return Ok(Vec::new());
    };
    Ok(one_edge("sh.tangled.repo.pull.comment", subject, source))
}

fn pull_status_edges(
    source: &AtUri<DefaultStr>,
    record: &PullStatusRecord<DefaultStr>,
) -> Result<Vec<Edge>, ExtractError> {
    let Some(subject) = uri_subject_for_record(&record.pull) else {
        return Ok(Vec::new());
    };
    Ok(one_edge("sh.tangled.repo.pull.status", subject, source))
}

fn vouch_edges(
    source: &AtUri<DefaultStr>,
    _record: &Vouch<DefaultStr>,
) -> Result<Vec<Edge>, ExtractError> {
    let Some(rkey) = source.rkey() else {
        return Ok(Vec::new());
    };
    let Ok(vouchee) = Did::<DefaultStr>::new_owned(rkey.as_ref()) else {
        return Ok(Vec::new());
    };
    Ok(one_edge(
        "sh.tangled.graph.vouch",
        SubjectRef::Did(vouchee),
        source,
    ))
}

fn spindle_member_edges(
    source: &AtUri<DefaultStr>,
    record: &SpindleMemberRecord<DefaultStr>,
) -> Result<Vec<Edge>, ExtractError> {
    Ok(one_edge(
        "sh.tangled.spindle.member",
        SubjectRef::Did(record.subject.clone()),
        source,
    ))
}

fn pipeline_edges(
    source: &AtUri<DefaultStr>,
    record: &Pipeline<DefaultStr>,
) -> Result<Vec<Edge>, ExtractError> {
    let trigger_repo = &record.trigger_metadata.repo;
    let repo_did = trigger_repo
        .repo_did
        .as_ref()
        .unwrap_or(&trigger_repo.did)
        .clone();
    Ok(one_edge(
        "sh.tangled.pipeline",
        SubjectRef::Did(repo_did),
        source,
    ))
}

fn owner_self_edges(kind: &'static str, source: &AtUri<DefaultStr>) -> Vec<Edge> {
    crate::ids::owner_did_from_aturi(source)
        .map(|did| one_edge(kind, SubjectRef::Did(did), source))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jacquard_common::types::nsid::Nsid;
    use serde_json::json;

    fn at(s: &str) -> AtUri<DefaultStr> {
        AtUri::new_owned(s).unwrap()
    }

    fn did(s: &str) -> Did<DefaultStr> {
        Did::new_owned(s).unwrap()
    }

    fn did_subj(s: &str) -> SubjectRef {
        SubjectRef::Did(did(s))
    }

    fn uri_subj(s: &str) -> SubjectRef {
        SubjectRef::Uri(at(s))
    }

    fn nsid(s: &'static str) -> Nsid<DefaultStr> {
        Nsid::new_static(s).unwrap()
    }

    fn extract(collection: &'static str, source: &str, body: serde_json::Value) -> Vec<Edge> {
        let parsed = Record::from_json_value(&nsid(collection), body).expect("parse");
        parsed.primary_edges(&at(source)).expect("extract")
    }

    #[test]
    fn star_repo_variant_keys_on_did() {
        let edges = extract(
            "sh.tangled.feed.star",
            "at://did:plc:olaren/sh.tangled.feed.star/abcabcabcabcz",
            json!({
                "$type": "sh.tangled.feed.star",
                "createdAt": "2026-05-01T00:00:00Z",
                "subject": {
                    "$type": "sh.tangled.feed.star#repo",
                    "did": "did:plc:abalone"
                }
            }),
        );
        assert_eq!(
            edges,
            vec![Edge {
                kind: nsid("sh.tangled.feed.star"),
                subject: did_subj("did:plc:abalone"),
                source: at("at://did:plc:olaren/sh.tangled.feed.star/abcabcabcabcz"),
                sort_micros: 0,
            }]
        );
    }

    #[test]
    fn star_string_variant_keeps_full_path() {
        let edges = extract(
            "sh.tangled.feed.star",
            "at://did:plc:olaren/sh.tangled.feed.star/abcabcabcabcz",
            json!({
                "$type": "sh.tangled.feed.star",
                "createdAt": "2026-05-01T00:00:00Z",
                "subject": {
                    "$type": "sh.tangled.feed.star#string",
                    "uri": "at://did:plc:teq/sh.tangled.string/k1"
                }
            }),
        );
        assert_eq!(edges.len(), 1);
        assert_eq!(
            edges[0].subject,
            uri_subj("at://did:plc:teq/sh.tangled.string/k1")
        );
    }

    #[test]
    fn star_string_variant_handle_authority_emits_no_edge() {
        let edges = extract(
            "sh.tangled.feed.star",
            "at://did:plc:olaren/sh.tangled.feed.star/abcabcabcabcz",
            json!({
                "$type": "sh.tangled.feed.star",
                "createdAt": "2026-05-01T00:00:00Z",
                "subject": {
                    "$type": "sh.tangled.feed.star#string",
                    "uri": "at://oyster.cafe/sh.tangled.repo/r1"
                }
            }),
        );
        assert!(edges.is_empty());
    }

    #[test]
    fn follow_uses_subject_did() {
        let edges = extract(
            "sh.tangled.graph.follow",
            "at://did:plc:olaren/sh.tangled.graph.follow/abcabcabcabcz",
            json!({
                "$type": "sh.tangled.graph.follow",
                "createdAt": "2026-05-01T00:00:00Z",
                "subject": "did:plc:bailey"
            }),
        );
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, nsid("sh.tangled.graph.follow"));
        assert_eq!(edges[0].subject, did_subj("did:plc:bailey"));
    }

    #[test]
    fn issue_keys_on_repo_did() {
        let edges = extract(
            "sh.tangled.repo.issue",
            "at://did:plc:nel/sh.tangled.repo.issue/abcabcabcabcz",
            json!({
                "$type": "sh.tangled.repo.issue",
                "repo": "did:plc:abalone",
                "title": "bug",
                "createdAt": "2026-05-01T00:00:00Z"
            }),
        );
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].subject, did_subj("did:plc:abalone"));
    }

    #[test]
    fn issue_comment_uses_issue_uri() {
        let edges = extract(
            "sh.tangled.repo.issue.comment",
            "at://did:plc:nel/sh.tangled.repo.issue.comment/abcabcabcabcz",
            json!({
                "$type": "sh.tangled.repo.issue.comment",
                "issue": "at://did:plc:nel/sh.tangled.repo.issue/3lk1",
                "body": "thoughts",
                "createdAt": "2026-05-01T00:00:00Z"
            }),
        );
        assert_eq!(edges.len(), 1);
        assert_eq!(
            edges[0].subject,
            uri_subj("at://did:plc:nel/sh.tangled.repo.issue/3lk1")
        );
    }

    #[test]
    fn pull_keys_on_target_repo_did() {
        let edges = extract(
            "sh.tangled.repo.pull",
            "at://did:plc:nel/sh.tangled.repo.pull/abcabcabcabcz",
            json!({
                "$type": "sh.tangled.repo.pull",
                "title": "feature",
                "createdAt": "2026-05-01T00:00:00Z",
                "rounds": [],
                "target": {"repo": "did:plc:abalone", "branch": "main"}
            }),
        );
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].subject, did_subj("did:plc:abalone"));
    }

    fn artifact_body(
        repo: Option<&AtUri<DefaultStr>>,
        repo_did: Option<&Did<DefaultStr>>,
    ) -> serde_json::Value {
        let mut body = json!({
            "$type": "sh.tangled.repo.artifact",
            "createdAt": "2026-05-01T00:00:00Z",
            "name": "out.bin",
            "tag": {"$bytes": "AAAAAAAAAAAAAAAAAAAAAAAAAAA="},
            "artifact": {
                "$type": "blob",
                "ref": {"$link": "bafkreigh2akiscaildc7gnvtklbsfhdgwz72eolmpckbqr5ej26byp3uli"},
                "mimeType": "application/octet-stream",
                "size": 12
            }
        });
        let obj = body.as_object_mut().expect("artifact_body returns object");
        if let Some(r) = repo {
            obj.insert("repo".into(), json!(r.as_ref()));
        }
        if let Some(d) = repo_did {
            obj.insert("repoDid".into(), json!(d.as_ref()));
        }
        body
    }

    #[test]
    fn artifact_prefers_repo_did_over_repo_uri() {
        let edges = extract(
            "sh.tangled.repo.artifact",
            "at://did:plc:nel/sh.tangled.repo.artifact/abcabcabcabcz",
            artifact_body(
                Some(&at("at://did:plc:abalone/sh.tangled.repo/r1")),
                Some(&did("did:plc:lyna")),
            ),
        );
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].subject, did_subj("did:plc:lyna"));
    }

    #[test]
    fn artifact_did_only_keys_on_did() {
        let edges = extract(
            "sh.tangled.repo.artifact",
            "at://did:plc:nel/sh.tangled.repo.artifact/abcabcabcabcz",
            artifact_body(None, Some(&did("did:plc:abalone"))),
        );
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].subject, did_subj("did:plc:abalone"));
    }

    #[test]
    fn artifact_without_repo_emits_no_edge() {
        let edges = extract(
            "sh.tangled.repo.artifact",
            "at://did:plc:nel/sh.tangled.repo.artifact/abcabcabcabcz",
            artifact_body(None, None),
        );
        assert!(edges.is_empty());
    }

    #[test]
    fn artifact_uri_only_is_preserved_for_normalization() {
        let edges = extract(
            "sh.tangled.repo.artifact",
            "at://did:plc:nel/sh.tangled.repo.artifact/abcabcabcabcz",
            artifact_body(Some(&at("at://did:plc:abalone/sh.tangled.repo/r1")), None),
        );
        assert_eq!(edges.len(), 1);
        assert_eq!(
            edges[0].subject,
            uri_subj("at://did:plc:abalone/sh.tangled.repo/r1"),
        );
    }

    #[test]
    fn artifact_uri_with_handle_authority_emits_no_edge() {
        let edges = extract(
            "sh.tangled.repo.artifact",
            "at://did:plc:nel/sh.tangled.repo.artifact/abcabcabcabcz",
            artifact_body(Some(&at("at://oyster.cafe/sh.tangled.repo/r1")), None),
        );
        assert!(edges.is_empty());
    }

    #[test]
    fn collaborator_keys_on_repo_did() {
        let edges = extract(
            "sh.tangled.repo.collaborator",
            "at://did:plc:nel/sh.tangled.repo.collaborator/abcabcabcabcz",
            json!({
                "$type": "sh.tangled.repo.collaborator",
                "repo": "did:plc:abalone",
                "subject": "did:plc:lyna",
                "createdAt": "2026-05-01T00:00:00Z"
            }),
        );
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, nsid("sh.tangled.repo.collaborator"));
        assert_eq!(edges[0].subject, did_subj("did:plc:abalone"));
    }

    #[test]
    fn ref_update_keys_on_repo_did() {
        let edges = extract(
            "sh.tangled.git.refUpdate",
            "at://did:plc:nel/sh.tangled.git.refUpdate/abcabcabcabcz",
            json!({
                "$type": "sh.tangled.git.refUpdate",
                "ref": "refs/heads/main",
                "committerDid": "did:plc:lyna",
                "repo": "did:plc:abalone",
                "oldSha": "0000000000000000000000000000000000000000",
                "newSha": "1111111111111111111111111111111111111111",
                "meta": {
                    "isDefaultRef": true,
                    "commitCount": {}
                }
            }),
        );
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, nsid("sh.tangled.git.refUpdate"));
        assert_eq!(edges[0].subject, did_subj("did:plc:abalone"));
    }

    #[test]
    fn label_op_keys_on_subject_at_uri() {
        let issue_uri = "at://did:plc:abalone/sh.tangled.repo.issue/i1";
        let edges = extract(
            "sh.tangled.label.op",
            "at://did:plc:nel/sh.tangled.label.op/abcabcabcabcz",
            json!({
                "$type": "sh.tangled.label.op",
                "performedAt": "2026-05-01T00:00:00Z",
                "subject": issue_uri,
                "add": [{
                    "key": "at://did:plc:abalone/sh.tangled.label.definition/bug",
                    "value": "true"
                }],
                "delete": []
            }),
        );
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, nsid("sh.tangled.label.op"));
        assert_eq!(edges[0].subject, uri_subj(issue_uri));
    }

    #[test]
    fn label_op_pull_subject_keys_on_pull_uri() {
        let pull_uri = "at://did:plc:abalone/sh.tangled.repo.pull/p1";
        let edges = extract(
            "sh.tangled.label.op",
            "at://did:plc:nel/sh.tangled.label.op/abcabcabcabcz",
            json!({
                "$type": "sh.tangled.label.op",
                "performedAt": "2026-05-01T00:00:00Z",
                "subject": pull_uri,
                "add": [],
                "delete": []
            }),
        );
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].subject, uri_subj(pull_uri));
    }

    #[test]
    fn pipeline_keys_on_repo_did_when_present() {
        let edges = extract(
            "sh.tangled.pipeline",
            "at://did:plc:lyna/sh.tangled.pipeline/pl1",
            json!({
                "$type": "sh.tangled.pipeline",
                "workflows": [],
                "triggerMetadata": {
                    "kind": "manual",
                    "repo": {
                        "did": "did:plc:nel",
                        "repoDid": "did:plc:abalone",
                        "knot": "oyster.cafe",
                        "defaultBranch": "main"
                    }
                }
            }),
        );
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, nsid("sh.tangled.pipeline"));
        assert_eq!(edges[0].subject, did_subj("did:plc:abalone"));
    }

    #[test]
    fn pipeline_falls_back_to_owner_did_when_repo_did_absent() {
        let edges = extract(
            "sh.tangled.pipeline",
            "at://did:plc:lyna/sh.tangled.pipeline/pl1",
            json!({
                "$type": "sh.tangled.pipeline",
                "workflows": [],
                "triggerMetadata": {
                    "kind": "manual",
                    "repo": {
                        "did": "did:plc:nel",
                        "knot": "oyster.cafe",
                        "defaultBranch": "main"
                    }
                }
            }),
        );
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, nsid("sh.tangled.pipeline"));
        assert_eq!(edges[0].subject, did_subj("did:plc:nel"));
    }

    #[test]
    fn pipeline_status_keys_on_pipeline_at_uri() {
        let pipeline_uri = "at://did:plc:lyna/sh.tangled.pipeline/pl1";
        let edges = extract(
            "sh.tangled.pipeline.status",
            "at://did:plc:bailey/sh.tangled.pipeline.status/abcabcabcabcz",
            json!({
                "$type": "sh.tangled.pipeline.status",
                "createdAt": "2026-05-01T00:00:00Z",
                "pipeline": pipeline_uri,
                "workflow": pipeline_uri,
                "status": "success"
            }),
        );
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, nsid("sh.tangled.pipeline.status"));
        assert_eq!(edges[0].subject, uri_subj(pipeline_uri));
    }

    #[test]
    fn knot_member_keys_on_subject_did() {
        let edges = extract(
            "sh.tangled.knot.member",
            "at://did:plc:teq/sh.tangled.knot.member/abcabcabcabcz",
            json!({
                "$type": "sh.tangled.knot.member",
                "createdAt": "2026-05-01T00:00:00Z",
                "subject": "did:plc:nel",
                "domain": "oyster.cafe"
            }),
        );
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, nsid("sh.tangled.knot.member"));
        assert_eq!(edges[0].subject, did_subj("did:plc:nel"));
    }

    #[test]
    fn spindle_member_keys_on_subject_did() {
        let edges = extract(
            "sh.tangled.spindle.member",
            "at://did:plc:teq/sh.tangled.spindle.member/abcabcabcabcz",
            json!({
                "$type": "sh.tangled.spindle.member",
                "createdAt": "2026-05-01T00:00:00Z",
                "subject": "did:plc:olaren",
                "instance": "spin.nel.pet"
            }),
        );
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, nsid("sh.tangled.spindle.member"));
        assert_eq!(edges[0].subject, did_subj("did:plc:olaren"));
    }

    #[test]
    fn repo_self_edge_keys_on_owner_did() {
        let edges = extract(
            "sh.tangled.repo",
            "at://did:plc:teq/sh.tangled.repo/r1",
            json!({
                "$type": "sh.tangled.repo",
                "name": "abalone",
                "knot": "oyster.cafe",
                "createdAt": "2026-05-01T00:00:00Z"
            }),
        );
        assert_eq!(
            edges,
            vec![Edge {
                kind: nsid("sh.tangled.repo"),
                subject: did_subj("did:plc:teq"),
                source: at("at://did:plc:teq/sh.tangled.repo/r1"),
                sort_micros: 0,
            }]
        );
    }

    #[test]
    fn knot_spindle_publickey_string_label_all_self_edge_to_owner() {
        let cases = [
            (
                "sh.tangled.knot",
                json!({"$type": "sh.tangled.knot", "createdAt": "2026-05-01T00:00:00Z"}),
            ),
            (
                "sh.tangled.spindle",
                json!({"$type": "sh.tangled.spindle", "createdAt": "2026-05-01T00:00:00Z"}),
            ),
            (
                "sh.tangled.publicKey",
                json!({"$type": "sh.tangled.publicKey", "createdAt": "2026-05-01T00:00:00Z", "key": "ssh-ed25519 AAA", "name": "laptop"}),
            ),
            (
                "sh.tangled.string",
                json!({"$type": "sh.tangled.string", "createdAt": "2026-05-01T00:00:00Z", "filename": "f.txt", "description": "x", "contents": "hi"}),
            ),
            (
                "sh.tangled.label.definition",
                json!({
                    "$type": "sh.tangled.label.definition",
                    "createdAt": "2026-05-01T00:00:00Z",
                    "name": "bug",
                    "valueType": {"type": "boolean", "format": "any"},
                    "scope": ["sh.tangled.repo.issue"]
                }),
            ),
        ];
        cases.into_iter().for_each(|(collection, body)| {
            let source = format!("at://did:plc:teq/{collection}/r1");
            let edges = extract(collection, &source, body);
            assert_eq!(edges.len(), 1, "{collection} must produce one self-edge");
            assert_eq!(edges[0].kind, nsid(collection));
            assert_eq!(edges[0].subject, did_subj("did:plc:teq"));
        });
    }

    #[test]
    fn profile_and_vouch_emit_no_edges() {
        let profile = extract(
            "sh.tangled.actor.profile",
            "at://did:plc:teq/sh.tangled.actor.profile/self",
            json!({"$type": "sh.tangled.actor.profile", "bluesky": false}),
        );
        assert!(profile.is_empty());
        let vouch = extract(
            "sh.tangled.graph.vouch",
            "at://did:plc:teq/sh.tangled.graph.vouch/r1",
            json!({"$type": "sh.tangled.graph.vouch", "createdAt": "2026-05-01T00:00:00Z", "kind": "vouch"}),
        );
        assert!(vouch.is_empty());
    }

    #[test]
    fn unknown_collection_errors_with_unknown_collection() {
        let err = Record::from_json_value(
            &nsid("app.bsky.feed.post"),
            json!({"$type": "app.bsky.feed.post", "text": "hi"}),
        )
        .expect_err("foreign collection");
        assert!(matches!(err, ExtractError::UnknownCollection(_)));
    }
}
