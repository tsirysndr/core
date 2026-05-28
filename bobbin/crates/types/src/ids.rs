use jacquard_common::DefaultStr;
use jacquard_common::types::did::Did;
use jacquard_common::types::nsid::Nsid;
use jacquard_common::types::recordkey::Rkey;
use jacquard_common::types::string::AtUri;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum SubjectRef {
    Did(Did<DefaultStr>),
    Uri(AtUri<DefaultStr>),
}

impl SubjectRef {
    pub fn from_did(did: Did<DefaultStr>) -> Self {
        Self::Did(did)
    }

    pub fn from_uri(uri: AtUri<DefaultStr>) -> Self {
        Self::Uri(uri)
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Did(d) => d.as_ref(),
            Self::Uri(u) => u.as_ref(),
        }
    }

    pub fn as_did(&self) -> Option<&Did<DefaultStr>> {
        match self {
            Self::Did(d) => Some(d),
            Self::Uri(_) => None,
        }
    }

    pub fn as_uri(&self) -> Option<&AtUri<DefaultStr>> {
        match self {
            Self::Uri(u) => Some(u),
            Self::Did(_) => None,
        }
    }
}

impl From<Did<DefaultStr>> for SubjectRef {
    fn from(did: Did<DefaultStr>) -> Self {
        Self::Did(did)
    }
}

impl From<AtUri<DefaultStr>> for SubjectRef {
    fn from(uri: AtUri<DefaultStr>) -> Self {
        Self::Uri(uri)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct EdgeKey {
    pub kind: Nsid<DefaultStr>,
    pub subject: SubjectRef,
}

impl EdgeKey {
    pub fn new(kind: Nsid<DefaultStr>, subject: SubjectRef) -> Self {
        Self { kind, subject }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RepoIdent {
    pub owner: Did<DefaultStr>,
    pub rkey: Rkey<DefaultStr>,
}

impl RepoIdent {
    pub fn new(owner: Did<DefaultStr>, rkey: Rkey<DefaultStr>) -> Self {
        Self { owner, rkey }
    }
}

pub fn nsid_static(s: &'static str) -> Nsid<DefaultStr> {
    Nsid::new_static(s).expect("compile-time NSID literal must validate")
}

pub fn owner_did_from_aturi(uri: &AtUri<DefaultStr>) -> Option<Did<DefaultStr>> {
    use jacquard_common::IntoStatic;
    use jacquard_common::types::ident::AtIdentifier;
    match uri.authority() {
        AtIdentifier::Did(d) => Some(d.into_static()),
        AtIdentifier::Handle(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn did(s: &'static str) -> Did<DefaultStr> {
        Did::new_static(s).unwrap()
    }

    fn at(s: &str) -> AtUri<DefaultStr> {
        AtUri::new_owned(s).unwrap()
    }

    #[test]
    fn owner_did_from_full_uri() {
        assert_eq!(
            owner_did_from_aturi(&at("at://did:plc:olaren/sh.tangled.feed.star/3lk1")),
            Some(did("did:plc:olaren")),
        );
    }

    #[test]
    fn owner_did_from_did_only_uri() {
        assert_eq!(
            owner_did_from_aturi(&at("at://did:plc:olaren")),
            Some(did("did:plc:olaren"))
        );
    }

    #[test]
    fn owner_did_rejects_handle_authority() {
        assert_eq!(
            owner_did_from_aturi(&at("at://oyster.cafe/sh.tangled.repo/r1")),
            None
        );
    }

    #[test]
    fn subject_ref_did_and_uri_hash_distinct() {
        use std::collections::HashSet;
        let mut s = HashSet::new();
        s.insert(SubjectRef::Did(did("did:plc:squid")));
        s.insert(SubjectRef::Uri(
            AtUri::new_owned("at://did:plc:squid").unwrap(),
        ));
        assert_eq!(
            s.len(),
            2,
            "did variant and uri variant must hash distinctly"
        );
    }
}
