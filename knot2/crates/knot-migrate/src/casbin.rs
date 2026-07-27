use std::collections::{BTreeMap, BTreeSet};

use crate::source::{AclRow, SourceDid, SourceRepoDid, SourceRepoName, SourceRepoObject};

const DOMAIN: &str = "thisserver";
const SKIPPED_ROLES: [&str; 6] = [
    "repo:push",
    "repo:settings",
    "repo:invite",
    "repo:delete",
    "repo:create",
    "server:invite",
];

#[derive(Debug, thiserror::Error)]
pub enum CasbinError {
    #[error("unrecognized acl row: {p_type},{v0},{v1},{v2},{v3}")]
    UnrecognizedRow {
        p_type: String,
        v0: String,
        v1: String,
        v2: String,
        v3: String,
    },
    #[error("acl names two server owners: {first} and {second}")]
    TwoServerOwners { first: SourceDid, second: SourceDid },
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct AclRoster {
    pub server_owner: Option<SourceDid>,
    pub members: BTreeSet<SourceDid>,
    pub owner_markers: BTreeMap<SourceRepoDid, BTreeSet<SourceDid>>,
    pub collaborators: BTreeMap<SourceRepoDid, BTreeSet<SourceDid>>,
    pub slash_collaborators: BTreeMap<SourceRepoDid, BTreeSet<SourceDid>>,
    pub slash_owner_markers: u64,
    pub slash_collab_rows: u64,
    pub unresolved_slash_forms: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashTarget {
    Unique(SourceRepoDid),
    Ambiguous,
}

pub type SlashResolver = BTreeMap<(SourceDid, SourceRepoName), SlashTarget>;

pub fn resolver(
    pairs: impl Iterator<Item = (SourceDid, SourceRepoName, SourceRepoDid)>,
) -> SlashResolver {
    pairs.fold(BTreeMap::new(), |mut map, (owner, name, repo)| {
        map.entry((owner, name))
            .and_modify(|target| *target = SlashTarget::Ambiguous)
            .or_insert(SlashTarget::Unique(repo));
        map
    })
}

pub fn decode(rows: &[AclRow], resolve: &SlashResolver) -> Result<AclRoster, CasbinError> {
    rows.iter().try_fold(AclRoster::default(), |roster, row| {
        step(roster, row, resolve)
    })
}

fn step(
    mut roster: AclRoster,
    row: &AclRow,
    resolve: &SlashResolver,
) -> Result<AclRoster, CasbinError> {
    match (
        row.p_type.as_str(),
        row.v0.as_str(),
        row.v1.as_str(),
        row.v2.as_str(),
        row.v3.as_str(),
    ) {
        // loadbearing ordering because of how old knots used to work
        ("g", "server:owner", "server:member", DOMAIN, _) => Ok(roster),
        ("g", did, "server:member", DOMAIN, _) => {
            roster.members.insert(SourceDid::from_column(did));
            Ok(roster)
        }
        ("g", did, "server:owner", DOMAIN, _) => match roster.server_owner.take() {
            Some(first) if first.as_str() != did => Err(CasbinError::TwoServerOwners {
                first,
                second: SourceDid::from_column(did),
            }),
            _ => {
                roster.server_owner = Some(SourceDid::from_column(did));
                Ok(roster)
            }
        },
        ("p", _, DOMAIN, _, role) if SKIPPED_ROLES.contains(&role) => Ok(roster),
        ("p", did, DOMAIN, object, "repo:owner") => Ok(mark(
            roster,
            Marker::Owner,
            &SourceDid::from_column(did),
            &SourceRepoObject::from_column(object),
            resolve,
        )),
        ("p", did, DOMAIN, object, "repo:collaborator") => Ok(mark(
            roster,
            Marker::Collaborator,
            &SourceDid::from_column(did),
            &SourceRepoObject::from_column(object),
            resolve,
        )),
        _ => Err(CasbinError::UnrecognizedRow {
            p_type: row.p_type.clone(),
            v0: row.v0.clone(),
            v1: row.v1.clone(),
            v2: row.v2.clone(),
            v3: row.v3.clone(),
        }),
    }
}

#[derive(Clone, Copy)]
enum Marker {
    Owner,
    Collaborator,
}

fn mark(
    mut roster: AclRoster,
    marker: Marker,
    did: &SourceDid,
    object: &SourceRepoObject,
    resolve: &SlashResolver,
) -> AclRoster {
    match (object.as_str().split_once('/'), marker) {
        (None, Marker::Owner) => {
            roster
                .owner_markers
                .entry(SourceRepoDid::from_column(object.as_str()))
                .or_default()
                .insert(did.clone());
        }
        (None, Marker::Collaborator) => {
            roster
                .collaborators
                .entry(SourceRepoDid::from_column(object.as_str()))
                .or_default()
                .insert(did.clone());
        }
        (Some((owner, name)), marker) => {
            match marker {
                Marker::Owner => roster.slash_owner_markers += 1,
                Marker::Collaborator => roster.slash_collab_rows += 1,
            }
            let resolved = resolve.get(&(
                SourceDid::from_column(owner),
                SourceRepoName::from_column(name),
            ));
            match (resolved, marker) {
                (Some(SlashTarget::Unique(repo)), Marker::Collaborator) => {
                    roster
                        .slash_collaborators
                        .entry(repo.clone())
                        .or_default()
                        .insert(did.clone());
                }
                (Some(SlashTarget::Unique(_)), Marker::Owner) => {}
                (Some(SlashTarget::Ambiguous), _) | (None, _) => {
                    roster.unresolved_slash_forms.push(object.to_string())
                }
            }
        }
    }
    roster
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(did: &str, role: &str) -> AclRow {
        AclRow {
            p_type: "g".into(),
            v0: did.into(),
            v1: role.into(),
            v2: DOMAIN.into(),
            v3: String::new(),
        }
    }

    fn p(did: &str, object: &str, role: &str) -> AclRow {
        AclRow {
            p_type: "p".into(),
            v0: did.into(),
            v1: DOMAIN.into(),
            v2: object.into(),
            v3: role.into(),
        }
    }

    #[test]
    fn decodes_members_owner_and_repo_markers() {
        let rows = [
            g("did:plc:nel", "server:owner"),
            g("server:owner", "server:member"),
            g("did:plc:olaren", "server:member"),
            g("did:plc:teq", "server:member"),
            p("did:plc:nel", "did:plc:squid", "repo:owner"),
            p("did:plc:nel", "did:plc:squid", "repo:push"),
            p("did:plc:nel", "did:plc:squid", "repo:settings"),
            p("did:plc:nel", "did:plc:squid", "repo:invite"),
            p("did:plc:nel", "did:plc:squid", "repo:delete"),
            p("did:plc:teq", "did:plc:squid", "repo:collaborator"),
            p("did:plc:nel", "", "repo:create"),
            p("did:plc:nel", "", "server:invite"),
        ];
        let roster = decode(&rows, &SlashResolver::new()).unwrap();
        assert_eq!(
            roster.server_owner.as_ref().map(SourceDid::as_str),
            Some("did:plc:nel")
        );
        assert_eq!(roster.members.len(), 2);
        assert_eq!(
            roster.owner_markers["did:plc:squid"],
            BTreeSet::from([SourceDid::from_column("did:plc:nel")])
        );
        assert_eq!(
            roster.collaborators["did:plc:squid"],
            BTreeSet::from([SourceDid::from_column("did:plc:teq")])
        );
    }

    #[test]
    fn slash_forms_resolve_through_repo_keys_unless_ambiguous() {
        let resolve = resolver(
            [
                ("did:plc:nel", "anemone", "did:plc:limpet"),
                ("did:plc:isabel", "mussel", "did:plc:whelk"),
                ("did:plc:isabel", "mussel", "did:plc:conch"),
            ]
            .into_iter()
            .map(|(owner, name, repo)| {
                (
                    SourceDid::from_column(owner),
                    SourceRepoName::from_column(name),
                    SourceRepoDid::from_column(repo),
                )
            }),
        );
        assert_eq!(
            resolve[&(
                SourceDid::from_column("did:plc:isabel"),
                SourceRepoName::from_column("mussel")
            )],
            SlashTarget::Ambiguous
        );

        let roster = decode(
            &[
                p("did:plc:nel", "did:plc:nel/anemone", "repo:owner"),
                p("did:plc:isabel", "did:plc:nel/anemone", "repo:collaborator"),
                p("did:plc:nel", "did:plc:nel/vanished", "repo:owner"),
                p("did:plc:teq", "did:plc:isabel/mussel", "repo:collaborator"),
            ],
            &resolve,
        )
        .unwrap();
        assert_eq!(roster.slash_owner_markers, 2);
        assert_eq!(roster.slash_collab_rows, 2);
        assert!(roster.owner_markers.is_empty());
        assert!(roster.collaborators.is_empty());
        assert!(roster.slash_collaborators["did:plc:limpet"].contains("did:plc:isabel"));
        assert_eq!(
            roster.slash_collaborators.len(),
            1,
            "an ambiguous slash form grants nobody"
        );
        assert_eq!(
            roster.unresolved_slash_forms,
            ["did:plc:nel/vanished", "did:plc:isabel/mussel"]
        );
    }

    #[test]
    fn decode_rejects_unknown_roles_and_two_server_owners() {
        assert!(matches!(
            decode(
                &[p("did:plc:nel", "did:plc:squid", "repo:mystery")],
                &SlashResolver::new()
            ),
            Err(CasbinError::UnrecognizedRow { .. })
        ));
        assert!(matches!(
            decode(
                &[
                    g("did:plc:nel", "server:owner"),
                    g("did:plc:bailey", "server:owner"),
                ],
                &SlashResolver::new()
            ),
            Err(CasbinError::TwoServerOwners { .. })
        ));
    }
}
