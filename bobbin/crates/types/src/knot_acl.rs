use core::fmt;

use jacquard_common::DefaultStr;
use jacquard_common::types::did::Did;
use jacquard_common::types::ident::AtIdentifier;
use jacquard_common::types::string::AtUri;

use crate::edges::Edge;
use crate::ids::{SubjectRef, nsid_static};

pub const KNOT_MEMBER_COLLECTION: &str = "sh.tangled.bobbin.knotMember";
pub const KNOT_COLLABORATOR_COLLECTION: &str = "sh.tangled.bobbin.knotCollaborator";

const KNOT_MEMBER_KIND: &str = "sh.tangled.knot.member";
const REPO_COLLABORATOR_KIND: &str = "sh.tangled.repo.collaborator";

const DID_WEB_PREFIX: &str = "did:web:";

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct KnotHostKey(String);

impl KnotHostKey {
    pub fn new(host: &str) -> Self {
        Self(host.trim_end_matches('.').to_ascii_lowercase())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for KnotHostKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for KnotHostKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KnotOwnedSource {
    Member {
        knot: Did<DefaultStr>,
        subject: Did<DefaultStr>,
    },
    Collaborator {
        repo: Did<DefaultStr>,
        subject: Did<DefaultStr>,
    },
}

pub fn host_to_knot_did(host: &str) -> Option<Did<DefaultStr>> {
    if host.contains('/') {
        return None;
    }
    let normalized = KnotHostKey::new(host);
    let host = normalized.as_str();
    if host.is_empty() {
        return None;
    }
    Did::new_owned(format!("{DID_WEB_PREFIX}{}", host.replace(':', "%3A"))).ok()
}

pub fn knot_did_host(knot: &Did<DefaultStr>) -> Option<String> {
    let encoded = knot.as_ref().strip_prefix(DID_WEB_PREFIX)?;
    if encoded.is_empty() {
        return None;
    }
    Some(encoded.replace("%3A", ":"))
}

pub fn member_source(
    knot: &Did<DefaultStr>,
    subject: &Did<DefaultStr>,
) -> Option<AtUri<DefaultStr>> {
    build_source(knot.as_ref(), KNOT_MEMBER_COLLECTION, subject.as_ref())
}

pub fn collaborator_source(
    repo: &Did<DefaultStr>,
    subject: &Did<DefaultStr>,
) -> Option<AtUri<DefaultStr>> {
    build_source(
        repo.as_ref(),
        KNOT_COLLABORATOR_COLLECTION,
        subject.as_ref(),
    )
}

pub fn member_upsert(
    knot: &Did<DefaultStr>,
    subject: &Did<DefaultStr>,
    created_micros: u64,
) -> Option<(AtUri<DefaultStr>, Vec<Edge>)> {
    let source = member_source(knot, subject)?;
    let edge = Edge {
        kind: nsid_static(KNOT_MEMBER_KIND),
        subject: SubjectRef::Did(subject.clone()),
        source: source.clone(),
        sort_micros: created_micros,
    };
    Some((source, vec![edge]))
}

pub fn collaborator_upsert(
    repo: &Did<DefaultStr>,
    subject: &Did<DefaultStr>,
    created_micros: u64,
) -> Option<(AtUri<DefaultStr>, Vec<Edge>)> {
    let source = collaborator_source(repo, subject)?;
    let edge = Edge {
        kind: nsid_static(REPO_COLLABORATOR_KIND),
        subject: SubjectRef::Did(repo.clone()),
        source: source.clone(),
        sort_micros: created_micros,
    };
    Some((source, vec![edge]))
}

pub fn decode_knot_owned_source(source: &AtUri<DefaultStr>) -> Option<KnotOwnedSource> {
    let collection = source.collection()?;
    let collection = collection.as_ref();
    if collection != KNOT_MEMBER_COLLECTION && collection != KNOT_COLLABORATOR_COLLECTION {
        return None;
    }
    let AtIdentifier::Did(authority) = source.authority() else {
        return None;
    };
    let subject = Did::new_owned(source.rkey()?.as_ref()).ok()?;
    match collection {
        KNOT_MEMBER_COLLECTION => Some(KnotOwnedSource::Member {
            knot: Did::new_owned(authority.as_ref()).ok()?,
            subject,
        }),
        KNOT_COLLABORATOR_COLLECTION => Some(KnotOwnedSource::Collaborator {
            repo: Did::new_owned(authority.as_ref()).ok()?,
            subject,
        }),
        _ => None,
    }
}

fn build_source(authority: &str, collection: &str, rkey: &str) -> Option<AtUri<DefaultStr>> {
    AtUri::new_owned(format!("at://{authority}/{collection}/{rkey}")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn did(s: &str) -> Did<DefaultStr> {
        Did::new_owned(s).unwrap()
    }

    fn at(s: &str) -> AtUri<DefaultStr> {
        AtUri::new_owned(s).unwrap()
    }

    #[test]
    fn host_did_round_trips() {
        let knot = host_to_knot_did("oyster.cafe").unwrap();
        assert_eq!(knot.as_ref(), "did:web:oyster.cafe");
        assert_eq!(knot_did_host(&knot), Some("oyster.cafe".to_owned()));
    }

    #[test]
    fn host_did_round_trips_with_port() {
        let knot = host_to_knot_did("oyster.cafe:3000").unwrap();
        assert_eq!(knot.as_ref(), "did:web:oyster.cafe%3A3000");
        assert_eq!(knot_did_host(&knot), Some("oyster.cafe:3000".to_owned()));
    }

    #[test]
    fn host_rejects_slash_and_empty() {
        assert_eq!(host_to_knot_did("oyster.cafe/evil"), None);
        assert_eq!(host_to_knot_did(""), None);
        assert_eq!(host_to_knot_did("."), None);
    }

    #[test]
    fn host_key_normalizes_case_and_trailing_dot() {
        assert_eq!(
            KnotHostKey::new("Kt.Oyster.Cafe.").as_str(),
            "kt.oyster.cafe"
        );
        assert_eq!(
            KnotHostKey::new("KT.OYSTER.CAFE:3000").as_str(),
            "kt.oyster.cafe:3000"
        );
        assert_eq!(
            KnotHostKey::new("kt.oyster.cafe"),
            KnotHostKey::new("KT.OYSTER.CAFE")
        );
    }

    #[test]
    fn host_to_knot_did_normalizes_before_encoding() {
        assert_eq!(
            host_to_knot_did("KT.Oyster.Cafe").unwrap().as_ref(),
            "did:web:kt.oyster.cafe"
        );
        assert_eq!(
            host_to_knot_did("KT.Oyster.Cafe:3000").unwrap().as_ref(),
            "did:web:kt.oyster.cafe%3A3000"
        );
    }

    #[test]
    fn member_source_round_trips() {
        let knot = host_to_knot_did("oyster.cafe").unwrap();
        let subject = did("did:plc:nel");
        let source = member_source(&knot, &subject).expect("build member source");
        assert_eq!(
            source.as_ref(),
            "at://did:web:oyster.cafe/sh.tangled.bobbin.knotMember/did:plc:nel"
        );
        assert_eq!(
            decode_knot_owned_source(&source),
            Some(KnotOwnedSource::Member { knot, subject })
        );
    }

    #[test]
    fn collaborator_source_round_trips() {
        let repo = did("did:plc:scallop");
        let subject = did("did:plc:olaren");
        let source = collaborator_source(&repo, &subject).expect("build collaborator source");
        assert_eq!(
            source.as_ref(),
            "at://did:plc:scallop/sh.tangled.bobbin.knotCollaborator/did:plc:olaren"
        );
        assert_eq!(
            decode_knot_owned_source(&source),
            Some(KnotOwnedSource::Collaborator { repo, subject })
        );
    }

    #[test]
    fn decode_ignores_legacy_pds_member_record() {
        let source = at("at://did:plc:nel/sh.tangled.knot.member/abcabcabcabcz");
        assert_eq!(decode_knot_owned_source(&source), None);
    }

    #[test]
    fn decode_ignores_handle_authority() {
        let source = at("at://oyster.cafe/sh.tangled.bobbin.knotMember/did:plc:nel");
        assert_eq!(decode_knot_owned_source(&source), None);
    }

    #[test]
    fn member_upsert_builds_decodable_primary_edge() {
        let knot = host_to_knot_did("oyster.cafe").unwrap();
        let subject = did("did:plc:nel");
        let (source, edges) =
            member_upsert(&knot, &subject, 1_700_000_000_000_000).expect("build member upsert");
        assert_eq!(edges.len(), 1);
        let edge = &edges[0];
        assert_eq!(edge.kind.as_ref(), "sh.tangled.knot.member");
        assert_eq!(edge.subject, SubjectRef::Did(subject.clone()));
        assert_eq!(edge.source, source);
        assert_eq!(edge.sort_micros, 1_700_000_000_000_000);
        assert_eq!(
            decode_knot_owned_source(&source),
            Some(KnotOwnedSource::Member { knot, subject })
        );
    }

    #[test]
    fn collaborator_upsert_keys_on_repo_did() {
        let repo = did("did:plc:scallop");
        let subject = did("did:plc:olaren");
        let (source, edges) =
            collaborator_upsert(&repo, &subject, 42).expect("build collaborator upsert");
        assert_eq!(edges.len(), 1);
        let edge = &edges[0];
        assert_eq!(edge.kind.as_ref(), "sh.tangled.repo.collaborator");
        assert_eq!(edge.subject, SubjectRef::Did(repo.clone()));
        assert_eq!(edge.source, source);
        assert_eq!(edge.sort_micros, 42);
        assert_eq!(
            decode_knot_owned_source(&source),
            Some(KnotOwnedSource::Collaborator { repo, subject })
        );
    }
}
