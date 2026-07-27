use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use knot_types::{AccountDid, OwnerDid, ParseError, RepoDid, RepoName, RepoRkey, UnixSeconds};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::casbin::AclRoster;
use crate::source::{
    CollabRow, MemberRow, RepoRow, SourceDid, SourceKeyType, SourceRepoDid, SourceRepoName,
    SourceRkey, SourceTimestamp,
};

#[derive(Debug, thiserror::Error)]
pub enum MappingError {
    #[error("repo DID {value} doesn't parse: {source}")]
    BadRepoDid {
        value: SourceRepoDid,
        source: ParseError,
    },
    #[error("owner {value} on repo {repo} doesn't parse: {source}")]
    BadOwnerDid {
        repo: SourceRepoDid,
        value: SourceDid,
        source: ParseError,
    },
    #[error("member DID {value} doesn't parse: {source}")]
    BadMemberDid {
        value: SourceDid,
        source: ParseError,
    },
    #[error("collaborator DID {value} on repo {repo} doesn't parse: {source}")]
    BadCollaboratorDid {
        repo: SourceRepoDid,
        value: SourceDid,
        source: ParseError,
    },
    #[error("{context} timestamp {value} isn't RFC 3339")]
    BadTimestamp {
        context: &'static str,
        value: SourceTimestamp,
    },
    #[error("repo {repo} has a {key_type} signing key of {bytes} bytes instead of 32-byte k256")]
    BadSigningKey {
        repo: SourceRepoDid,
        key_type: SourceKeyType,
        bytes: usize,
    },
    #[error("acl names no server owner")]
    MissingServerOwner,
    #[error("acl marks {marker} as owner of {repo} while repo_keys names {owner}")]
    ConflictingOwnerMarker {
        repo: SourceRepoDid,
        marker: SourceDid,
        owner: SourceDid,
    },
    #[error("record key {owner}/{rkey} has no single alias-backed holder")]
    AmbiguousRkey { owner: OwnerDid, rkey: RepoRkey },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedGrant {
    pub subject: AccountDid,
    pub added_by: AccountDid,
    pub created_at: UnixSeconds,
    pub unioned: bool,
}

#[derive(Clone, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct SigningKey([u8; 32]);

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SigningKey").finish_non_exhaustive()
    }
}

impl SigningKey {
    pub fn to_hex(&self) -> zeroize::Zeroizing<String> {
        zeroize::Zeroizing::new(knot_types::lowercase_hex(&self.0))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptRepo {
    pub source_did: SourceRepoDid,
    pub did: RepoDid,
    pub owner: OwnerDid,
    pub name: RepoName,
    pub rkey: RepoRkey,
    pub created_at: UnixSeconds,
    pub signing_key: SigningKey,
    pub collaborators: Vec<MappedGrant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    Name { value: SourceRepoName },
    Rkey { value: SourceRkey },
    RkeyCollision { rkey: RepoRkey, winner: RepoDid },
    NoSourceRepo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedRepo {
    pub repo_did: RepoDid,
    pub reason: SkipReason,
    pub lost_collaborators: Vec<AccountDid>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Drift {
    pub acl_only_collaborators: Vec<(SourceRepoDid, SourceDid)>,
    pub table_only_collaborators: Vec<(SourceRepoDid, SourceDid)>,
    pub slash_resolved_collaborators: Vec<(SourceRepoDid, SourceDid)>,
    pub orphan_collaborator_pairs: Vec<(SourceRepoDid, SourceDid)>,
    pub markerless_owner_repos: Vec<SourceRepoDid>,
    pub orphan_owner_markers: Vec<SourceRepoDid>,
    pub extra_owner_markers: Vec<(SourceRepoDid, SourceDid)>,
    pub acl_only_members: Vec<SourceDid>,
    pub table_only_members: Vec<SourceDid>,
    pub slash_owner_markers: u64,
    pub slash_collab_rows: u64,
    pub unresolved_slash_forms: Vec<String>,
}

#[derive(Debug)]
pub struct Mapping {
    pub knot_owner: AccountDid,
    pub members: Vec<MappedGrant>,
    pub repos: Vec<AdoptRepo>,
    pub skipped: Vec<SkippedRepo>,
    pub drift: Drift,
}

pub fn map_tables(
    repos: &[RepoRow],
    rkeys: &BTreeMap<SourceRepoDid, SourceRkey>,
    members: &[MemberRow],
    collabs: &[CollabRow],
    acl: &AclRoster,
    exists: impl Fn(&SourceRepoDid) -> bool,
) -> Result<Mapping, MappingError> {
    let knot_owner = server_owner(acl)?;
    let repo_index: BTreeMap<&SourceRepoDid, &RepoRow> =
        repos.iter().map(|repo| (&repo.repo_did, repo)).collect();

    let table_members: Vec<MappedGrant> = members
        .iter()
        .filter(|row| row.subject.as_str() != knot_owner.as_str())
        .map(|row| {
            Ok(MappedGrant {
                subject: account(&row.subject).map_err(|source| MappingError::BadMemberDid {
                    value: row.subject.clone(),
                    source,
                })?,
                added_by: account(&row.did).map_err(|source| MappingError::BadMemberDid {
                    value: row.did.clone(),
                    source,
                })?,
                created_at: unix("knot_members.created", &row.created)?,
                unioned: false,
            })
        })
        .collect::<Result<_, MappingError>>()?;

    let table_member_dids: BTreeSet<&SourceDid> = members.iter().map(|row| &row.subject).collect();
    let acl_only_members: Vec<SourceDid> = acl
        .members
        .iter()
        .filter(|did| !table_member_dids.contains(*did) && did.as_str() != knot_owner.as_str())
        .cloned()
        .collect();
    let table_only_members: Vec<SourceDid> = members
        .iter()
        .filter(|row| !acl.members.contains(&row.subject))
        .map(|row| row.subject.clone())
        .collect();
    let unioned_members: Vec<MappedGrant> = acl_only_members
        .iter()
        .map(|did| {
            Ok(MappedGrant {
                subject: account(did).map_err(|source| MappingError::BadMemberDid {
                    value: did.clone(),
                    source,
                })?,
                added_by: knot_owner.clone(),
                // every casbin-only owner is at epoch
                created_at: UnixSeconds::new(0),
                unioned: true,
            })
        })
        .collect::<Result<_, MappingError>>()?;

    let (live_collabs, orphan_collabs): (Vec<&CollabRow>, Vec<&CollabRow>) = collabs
        .iter()
        .partition(|row| repo_index.contains_key(&row.repo_did));
    let table_pairs: BTreeSet<(&SourceRepoDid, &SourceDid)> = collabs
        .iter()
        .map(|row| (&row.repo_did, &row.subject_did))
        .collect();
    let acl_extra: Vec<(&SourceRepoDid, &SourceDid)> = acl
        .collaborators
        .iter()
        .flat_map(|(repo, dids)| dids.iter().map(move |did| (repo, did)))
        .filter(|(repo, did)| !table_pairs.contains(&(*repo, *did)))
        .collect();
    let (acl_only, acl_orphans): (Vec<_>, Vec<_>) = acl_extra
        .into_iter()
        .partition(|(repo, _)| repo_index.contains_key(*repo));
    let orphan_pairs: BTreeSet<(SourceRepoDid, SourceDid)> = acl_orphans
        .into_iter()
        .map(|(repo, did)| (repo.clone(), did.clone()))
        .chain(
            orphan_collabs
                .iter()
                .map(|row| (row.repo_did.clone(), row.subject_did.clone())),
        )
        .collect();
    let table_only_collaborators: Vec<(SourceRepoDid, SourceDid)> = live_collabs
        .iter()
        .filter(|row| {
            [&acl.collaborators, &acl.slash_collaborators]
                .into_iter()
                .all(|grants| {
                    grants
                        .get(&row.repo_did)
                        .is_none_or(|dids| !dids.contains(&row.subject_did))
                })
        })
        .map(|row| (row.repo_did.clone(), row.subject_did.clone()))
        .collect();

    let table_grants = live_collabs.iter().copied().try_fold(
        BTreeMap::<SourceRepoDid, Vec<MappedGrant>>::new(),
        |mut grants, row| {
            let grant = MappedGrant {
                subject: account(&row.subject_did).map_err(|source| {
                    MappingError::BadCollaboratorDid {
                        repo: row.repo_did.clone(),
                        value: row.subject_did.clone(),
                        source,
                    }
                })?,
                added_by: account(&row.added_by_did).map_err(|source| {
                    MappingError::BadCollaboratorDid {
                        repo: row.repo_did.clone(),
                        value: row.added_by_did.clone(),
                        source,
                    }
                })?,
                created_at: unix("collaborators.created", &row.created)?,
                unioned: false,
            };
            let entry = grants.entry(row.repo_did.clone()).or_default();
            if !entry.iter().any(|held| held.subject == grant.subject) {
                entry.push(grant);
            }
            Ok::<_, MappingError>(grants)
        },
    )?;
    let collab_grants = owner_attributed_grants(&acl_only, &repo_index, true, table_grants)?;

    let owners = owner_drift(repos, &repo_index, acl)?;
    let (adopted, skipped) = classify(repos, rkeys, &collab_grants, exists)?;

    Ok(Mapping {
        knot_owner,
        members: table_members.into_iter().chain(unioned_members).collect(),
        repos: adopted,
        skipped,
        drift: Drift {
            acl_only_collaborators: acl_only
                .into_iter()
                .map(|(repo, did)| (repo.clone(), did.clone()))
                .collect(),
            table_only_collaborators,
            slash_resolved_collaborators: acl
                .slash_collaborators
                .iter()
                .flat_map(|(repo, dids)| dids.iter().map(move |did| (repo, did)))
                .filter(|(repo, did)| !table_pairs.contains(&(*repo, *did)))
                .map(|(repo, did)| (repo.clone(), did.clone()))
                .collect(),
            orphan_collaborator_pairs: orphan_pairs.into_iter().collect(),
            markerless_owner_repos: owners.markerless,
            orphan_owner_markers: owners.orphans,
            extra_owner_markers: owners.extras,
            acl_only_members,
            table_only_members,
            slash_owner_markers: acl.slash_owner_markers,
            slash_collab_rows: acl.slash_collab_rows,
            unresolved_slash_forms: acl.unresolved_slash_forms.clone(),
        },
    })
}

pub fn map_preflip(
    repos: &[RepoRow],
    rkeys: &BTreeMap<SourceRepoDid, SourceRkey>,
    members: &[MemberRow],
    acl: &AclRoster,
    exists: impl Fn(&SourceRepoDid) -> bool,
) -> Result<Mapping, MappingError> {
    let knot_owner = server_owner(acl)?;
    let repo_index: BTreeMap<&SourceRepoDid, &RepoRow> =
        repos.iter().map(|repo| (&repo.repo_did, repo)).collect();
    let enrich: BTreeMap<&SourceDid, &MemberRow> =
        members.iter().map(|row| (&row.subject, row)).collect();
    let table_only_members: Vec<SourceDid> = members
        .iter()
        .filter(|row| !acl.members.contains(&row.subject))
        .map(|row| row.subject.clone())
        .collect();

    let mapped_members: Vec<MappedGrant> = acl
        .members
        .iter()
        .filter(|did| did.as_str() != knot_owner.as_str())
        .map(|did| {
            let subject = account(did).map_err(|source| MappingError::BadMemberDid {
                value: did.clone(),
                source,
            })?;
            match enrich.get(did) {
                Some(row) => Ok(MappedGrant {
                    subject,
                    added_by: account(&row.did).map_err(|source| MappingError::BadMemberDid {
                        value: row.did.clone(),
                        source,
                    })?,
                    created_at: unix("knot_members.created", &row.created)?,
                    unioned: false,
                }),
                None => Ok(MappedGrant {
                    subject,
                    added_by: knot_owner.clone(),
                    created_at: UnixSeconds::new(0),
                    unioned: false,
                }),
            }
        })
        .collect::<Result<_, MappingError>>()?;

    let combined: BTreeMap<&SourceRepoDid, BTreeSet<&SourceDid>> = acl
        .collaborators
        .iter()
        .chain(acl.slash_collaborators.iter())
        .flat_map(|(repo, dids)| dids.iter().map(move |did| (repo, did)))
        .fold(BTreeMap::new(), |mut pairs, (repo, did)| {
            pairs.entry(repo).or_default().insert(did);
            pairs
        });
    let live = combined
        .iter()
        .flat_map(|(repo, dids)| dids.iter().map(move |did| (*repo, *did)));
    let (resolvable, orphan_pairs): (Vec<_>, Vec<_>) =
        live.partition(|(repo, _)| repo_index.contains_key(*repo));
    let collab_grants = owner_attributed_grants(&resolvable, &repo_index, false, BTreeMap::new())?;

    let owners = owner_drift(repos, &repo_index, acl)?;
    let (adopted, skipped) = classify(repos, rkeys, &collab_grants, exists)?;

    Ok(Mapping {
        knot_owner,
        members: mapped_members,
        repos: adopted,
        skipped,
        drift: Drift {
            orphan_collaborator_pairs: orphan_pairs
                .into_iter()
                .map(|(repo, did)| (repo.clone(), did.clone()))
                .collect(),
            markerless_owner_repos: owners.markerless,
            orphan_owner_markers: owners.orphans,
            extra_owner_markers: owners.extras,
            table_only_members,
            slash_owner_markers: acl.slash_owner_markers,
            slash_collab_rows: acl.slash_collab_rows,
            unresolved_slash_forms: acl.unresolved_slash_forms.clone(),
            ..Drift::default()
        },
    })
}

fn owner_attributed_grants(
    pairs: &[(&SourceRepoDid, &SourceDid)],
    repo_index: &BTreeMap<&SourceRepoDid, &RepoRow>,
    unioned: bool,
    base: BTreeMap<SourceRepoDid, Vec<MappedGrant>>,
) -> Result<BTreeMap<SourceRepoDid, Vec<MappedGrant>>, MappingError> {
    pairs
        .iter()
        .copied()
        .try_fold(base, |mut grants, (repo, did)| {
            let row = repo_index[repo];
            let grant = MappedGrant {
                subject: account(did).map_err(|source| MappingError::BadCollaboratorDid {
                    repo: repo.clone(),
                    value: did.clone(),
                    source,
                })?,
                added_by: account(&row.owner_did).map_err(|source| MappingError::BadOwnerDid {
                    repo: repo.clone(),
                    value: row.owner_did.clone(),
                    source,
                })?,
                created_at: unix("repo_keys.created_at", &row.created_at)?,
                unioned,
            };
            grants.entry(repo.clone()).or_default().push(grant);
            Ok(grants)
        })
}

fn server_owner(acl: &AclRoster) -> Result<AccountDid, MappingError> {
    let did = acl
        .server_owner
        .as_ref()
        .ok_or(MappingError::MissingServerOwner)?;
    account(did).map_err(|source| MappingError::BadMemberDid {
        value: did.clone(),
        source,
    })
}

struct OwnerDrift {
    markerless: Vec<SourceRepoDid>,
    orphans: Vec<SourceRepoDid>,
    extras: Vec<(SourceRepoDid, SourceDid)>,
}

fn owner_drift(
    repos: &[RepoRow],
    repo_index: &BTreeMap<&SourceRepoDid, &RepoRow>,
    acl: &AclRoster,
) -> Result<OwnerDrift, MappingError> {
    repos.iter().try_for_each(|repo| {
        let conflicting = acl.owner_markers.get(&repo.repo_did).and_then(|markers| {
            (!markers.contains(&repo.owner_did))
                .then(|| markers.iter().next().cloned())
                .flatten()
        });
        match conflicting {
            Some(marker) => Err(MappingError::ConflictingOwnerMarker {
                repo: repo.repo_did.clone(),
                marker,
                owner: repo.owner_did.clone(),
            }),
            None => Ok(()),
        }
    })?;
    let markerless = repos
        .iter()
        .filter(|repo| !acl.owner_markers.contains_key(&repo.repo_did))
        .map(|repo| repo.repo_did.clone())
        .collect();
    let orphans = acl
        .owner_markers
        .keys()
        .filter(|repo| !repo_index.contains_key(*repo))
        .cloned()
        .collect();
    let extras = repos
        .iter()
        .filter_map(|repo| {
            acl.owner_markers
                .get(&repo.repo_did)
                .map(|markers| (repo, markers))
        })
        .flat_map(|(repo, markers)| {
            markers
                .iter()
                .filter(move |marker| *marker != &repo.owner_did)
                .map(move |marker| (repo.repo_did.clone(), marker.clone()))
        })
        .collect();
    Ok(OwnerDrift {
        markerless,
        orphans,
        extras,
    })
}

fn classify(
    repos: &[RepoRow],
    rkeys: &BTreeMap<SourceRepoDid, SourceRkey>,
    collab_grants: &BTreeMap<SourceRepoDid, Vec<MappedGrant>>,
    exists: impl Fn(&SourceRepoDid) -> bool,
) -> Result<(Vec<AdoptRepo>, Vec<SkippedRepo>), MappingError> {
    let (adopted, skipped) = repos.iter().try_fold(
        (Vec::new(), Vec::new()),
        |(mut adopted, mut skipped), row| {
            match classify_one(row, rkeys, collab_grants, &exists)? {
                Ok(repo) => adopted.push(repo),
                Err(skip) => skipped.push(skip),
            }
            Ok::<_, MappingError>((adopted, skipped))
        },
    )?;
    let alias_backed: BTreeSet<RepoDid> = rkeys
        .keys()
        .filter_map(|did| RepoDid::new(did.as_str()).ok())
        .collect();
    resolve_rkey_collisions(adopted, skipped, &alias_backed)
}

fn resolve_rkey_collisions(
    adopted: Vec<AdoptRepo>,
    skipped: Vec<SkippedRepo>,
    alias_backed: &BTreeSet<RepoDid>,
) -> Result<(Vec<AdoptRepo>, Vec<SkippedRepo>), MappingError> {
    let groups: BTreeMap<(&OwnerDid, &RepoRkey), Vec<usize>> =
        adopted
            .iter()
            .enumerate()
            .fold(BTreeMap::new(), |mut groups, (index, repo)| {
                groups
                    .entry((&repo.owner, &repo.rkey))
                    .or_default()
                    .push(index);
                groups
            });
    let losers: BTreeMap<usize, SkippedRepo> = groups
        .into_iter()
        .filter(|(_, indices)| indices.len() > 1)
        .map(|((owner, rkey), indices)| {
            let backed: Vec<usize> = indices
                .iter()
                .copied()
                .filter(|index| alias_backed.contains(&adopted[*index].did))
                .collect();
            match backed.as_slice() {
                [winner] => Ok(indices
                    .into_iter()
                    .filter(|index| index != &*winner)
                    .map(|index| {
                        (
                            index,
                            SkippedRepo {
                                repo_did: adopted[index].did.clone(),
                                reason: SkipReason::RkeyCollision {
                                    rkey: rkey.clone(),
                                    winner: adopted[*winner].did.clone(),
                                },
                                lost_collaborators: adopted[index]
                                    .collaborators
                                    .iter()
                                    .map(|grant| grant.subject.clone())
                                    .collect(),
                            },
                        )
                    })
                    .collect::<Vec<_>>()),
                _ => Err(MappingError::AmbiguousRkey {
                    owner: owner.clone(),
                    rkey: rkey.clone(),
                }),
            }
        })
        .collect::<Result<Vec<_>, MappingError>>()?
        .into_iter()
        .flatten()
        .collect();
    let (kept, demoted) = adopted.into_iter().enumerate().fold(
        (Vec::new(), losers),
        |(mut kept, demoted), (index, repo)| {
            if !demoted.contains_key(&index) {
                kept.push(repo);
            }
            (kept, demoted)
        },
    );
    Ok((
        kept,
        skipped.into_iter().chain(demoted.into_values()).collect(),
    ))
}

fn classify_one(
    row: &RepoRow,
    rkeys: &BTreeMap<SourceRepoDid, SourceRkey>,
    collab_grants: &BTreeMap<SourceRepoDid, Vec<MappedGrant>>,
    exists: &impl Fn(&SourceRepoDid) -> bool,
) -> Result<Result<AdoptRepo, SkippedRepo>, MappingError> {
    let did = RepoDid::new(row.repo_did.as_str()).map_err(|source| MappingError::BadRepoDid {
        value: row.repo_did.clone(),
        source,
    })?;
    let owner =
        OwnerDid::new(row.owner_did.as_str()).map_err(|source| MappingError::BadOwnerDid {
            repo: row.repo_did.clone(),
            value: row.owner_did.clone(),
            source,
        })?;
    let created_at = unix("repo_keys.created_at", &row.created_at)?;
    let key_bytes: [u8; 32] =
        row.signing_key
            .as_bytes()
            .try_into()
            .map_err(|_| MappingError::BadSigningKey {
                repo: row.repo_did.clone(),
                key_type: row.key_type.clone(),
                bytes: row.signing_key.as_bytes().len(),
            })?;
    if !row.key_type.is_k256() {
        return Err(MappingError::BadSigningKey {
            repo: row.repo_did.clone(),
            key_type: row.key_type.clone(),
            bytes: row.signing_key.as_bytes().len(),
        });
    }

    let skip = |reason: SkipReason| SkippedRepo {
        repo_did: did.clone(),
        reason,
        lost_collaborators: collab_grants
            .get(&row.repo_did)
            .map(|grants| grants.iter().map(|grant| grant.subject.clone()).collect())
            .unwrap_or_default(),
    };
    let name = match RepoName::new(row.repo_name.as_str()) {
        Ok(name) => name,
        Err(_) => {
            return Ok(Err(skip(SkipReason::Name {
                value: row.repo_name.clone(),
            })));
        }
    };
    let raw_rkey = rkeys
        .get(&row.repo_did)
        .map(SourceRkey::as_str)
        .unwrap_or(row.repo_name.as_str());
    let rkey = match RepoRkey::new(raw_rkey) {
        Ok(rkey) => rkey,
        Err(_) => {
            return Ok(Err(skip(SkipReason::Rkey {
                value: SourceRkey::from_column(raw_rkey),
            })));
        }
    };
    if !exists(&row.repo_did) {
        return Ok(Err(skip(SkipReason::NoSourceRepo)));
    }

    Ok(Ok(AdoptRepo {
        source_did: row.repo_did.clone(),
        did,
        owner,
        name,
        rkey,
        created_at,
        signing_key: SigningKey(key_bytes),
        collaborators: collab_grants
            .get(&row.repo_did)
            .cloned()
            .unwrap_or_default(),
    }))
}

fn account(value: &SourceDid) -> Result<AccountDid, ParseError> {
    AccountDid::new(value.as_str())
}

fn unix(context: &'static str, value: &SourceTimestamp) -> Result<UnixSeconds, MappingError> {
    chrono::DateTime::parse_from_rfc3339(value.as_str())
        .map(|parsed| UnixSeconds::new(parsed.timestamp()))
        .map_err(|_| MappingError::BadTimestamp {
            context,
            value: value.clone(),
        })
}
