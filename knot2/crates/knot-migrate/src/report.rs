use std::fmt::{self, Display, Formatter};

use crate::adopt::AdoptOutcome;
use crate::emit::CobSummary;
use crate::emit::HostKeyPlacement;
use crate::mapping::{Mapping, SkipReason};
use crate::rehearse::{Fit, Occupancy, Rehearsal};
use crate::source::{SourceDid, SourceRepoDid};

pub struct Report<'a> {
    pub mapping: &'a Mapping,
    pub orphan_alias_count: u64,
    pub phase: Phase<'a>,
}

pub enum Phase<'a> {
    Refused,
    Rehearsed(&'a Rehearsal),
    Written {
        adoption: &'a AdoptOutcome,
        cobs: &'a CobSummary,
    },
}

impl Display for Report<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mapping = self.mapping;
        let drift = &mapping.drift;
        writeln!(f, "knot owner: {}", mapping.knot_owner)?;
        writeln!(f, "members to grant: {}", mapping.members.len())?;
        writeln!(f, "repos to adopt: {}", mapping.repos.len())?;
        writeln!(
            f,
            "collaborator grants: {}",
            mapping
                .repos
                .iter()
                .map(|repo| repo.collaborators.len())
                .sum::<usize>()
        )?;
        writeln!(f)?;
        match drift.is_clean() && self.orphan_alias_count == 0 {
            true => writeln!(f, "casbin cross-check: the acl and the tables agree")?,
            false => {
                writeln!(f, "casbin cross-check drift:")?;
                listed(
                    f,
                    "acl-only collaborator grants unioned in",
                    drift.acl_only_collaborators.iter().map(GrantRow::from),
                )?;
                listed(
                    f,
                    "table-only collaborator grants missing from acl",
                    drift.table_only_collaborators.iter().map(GrantRow::from),
                )?;
                listed(
                    f,
                    "repos with no acl owner marker where the owner regains push",
                    drift.markerless_owner_repos.iter(),
                )?;
                counted(
                    f,
                    "orphan owner markers on unknown repos",
                    drift.orphan_owner_markers.len() as u64,
                )?;
                listed(
                    f,
                    "repos recorded with different owners in the acl and repo_keys",
                    drift.conflicting_owner_markers.iter(),
                )?;
                listed(
                    f,
                    "extra acl owner markers dropped",
                    drift.extra_owner_markers.iter().map(GrantRow::from),
                )?;
                counted(
                    f,
                    "orphan collaborator pairs on unknown repos",
                    drift.orphan_collaborator_pairs.len() as u64,
                )?;
                listed(
                    f,
                    "acl-only members unioned in",
                    drift.acl_only_members.iter(),
                )?;
                counted(
                    f,
                    "table-only members missing from acl",
                    drift.table_only_members.len() as u64,
                )?;
                counted(f, "slash-form owner markers", drift.slash_owner_markers)?;
                counted(f, "slash-form collaborator rows", drift.slash_collab_rows)?;
                listed(
                    f,
                    "slash-resolved collaborator grants left out of the union",
                    drift
                        .slash_resolved_collaborators
                        .iter()
                        .map(GrantRow::from),
                )?;
                listed(
                    f,
                    "unresolved slash forms",
                    drift.unresolved_slash_forms.iter(),
                )?;
                counted(f, "orphan aliases", self.orphan_alias_count)?;
            }
        }
        writeln!(f)?;
        writeln!(f, "skipped repos: {}", mapping.skipped.len())?;
        mapping.skipped.iter().try_for_each(|skip| {
            writeln!(f, "{} {}", skip.repo_did, describe(&skip.reason))?;
            skip.lost_collaborators
                .iter()
                .try_for_each(|did| writeln!(f, "drops collaborator grant for {did}"))
        })?;
        match self.phase {
            Phase::Refused => Ok(()),
            Phase::Rehearsed(rehearsal) => {
                writeln!(f)?;
                match &rehearsal.transfer {
                    Err(error) => writeln!(f, "transfer mode: {error}"),
                    Ok(transfer) => writeln!(f, "transfer mode: {transfer}"),
                }?;
                rehearsal.fallback.as_ref().map_or(Ok(()), |fallback| {
                    writeln!(
                        f,
                        "the filesystem checks used {}, since the scan path doesn't exist yet",
                        fallback.display()
                    )
                })?;
                match &rehearsal.scan_path {
                    Ok(Occupancy::Fresh) => writeln!(f, "scan path: writable"),
                    Ok(Occupancy::Occupied) => writeln!(
                        f,
                        "scan path: writable, with repos already in it that the migration will keep"
                    ),
                    Err(error) => writeln!(f, "scan path: {error}"),
                }?;
                rehearsal.room.as_ref().map_or(Ok(()), |room| match room {
                    Ok(room) => match room.fit() {
                        Fit::Clear => writeln!(
                            f,
                            "room to copy: {} free is enough for the {} that adoption will copy",
                            room.free, room.source
                        ),
                        Fit::Short => writeln!(
                            f,
                            "room to copy: {} free isn't enough for the {} that adoption will copy",
                            room.free, room.source
                        ),
                    },
                    Err(error) => writeln!(f, "room to copy: {error}"),
                })?;
                match &rehearsal.host_key {
                    Ok(algorithm) => writeln!(f, "host key algorithm: {algorithm}"),
                    Err(error) => writeln!(f, "host key: {error}"),
                }?;
                match &rehearsal.host_key_target {
                    None | Some(Ok(HostKeyPlacement::Fresh)) => Ok(()),
                    Some(Ok(HostKeyPlacement::Unchanged)) => {
                        writeln!(f, "host key: the target already has the imported key")
                    }
                    Some(Ok(HostKeyPlacement::Replacing)) => {
                        writeln!(
                            f,
                            "host key: the migration will replace the different key at the target"
                        )
                    }
                    Some(Err(error)) => writeln!(f, "host key: {error}"),
                }?;
                match &rehearsal.master_key {
                    Ok(env) => writeln!(f, "master key: {env} decodes to a usable key"),
                    Err(error) => writeln!(f, "{error}"),
                }
            }
            Phase::Written { adoption, cobs } => {
                writeln!(f)?;
                writeln!(
                    f,
                    "adopted by {}: {} new, {} already present, {} sha1, {} sha256",
                    adoption.transfer,
                    adoption.adopted,
                    adoption.already_present,
                    adoption.sha1,
                    adoption.sha256
                )?;
                writeln!(f)?;
                writeln!(
                    f,
                    "member grants: {} appended, {} already present",
                    cobs.members.appended, cobs.members.already_present
                )?;
                writeln!(
                    f,
                    "registrations: {} appended, {} already present",
                    cobs.registrations.appended, cobs.registrations.already_present
                )?;
                writeln!(
                    f,
                    "collaborator grants: {} appended, {} already present",
                    cobs.collaborators.appended, cobs.collaborators.already_present
                )
            }
        }
    }
}

struct GrantRow<'a>(&'a SourceRepoDid, &'a SourceDid);

impl<'a> From<&'a (SourceRepoDid, SourceDid)> for GrantRow<'a> {
    fn from((repo, did): &'a (SourceRepoDid, SourceDid)) -> Self {
        Self(repo, did)
    }
}

impl Display for GrantRow<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{} <- {}", self.0, self.1)
    }
}

fn counted(f: &mut Formatter<'_>, label: &str, count: u64) -> fmt::Result {
    match count {
        0 => Ok(()),
        count => writeln!(f, "{label}: {count}"),
    }
}

fn listed<T: Display>(
    f: &mut Formatter<'_>,
    label: &str,
    mut rows: impl ExactSizeIterator<Item = T>,
) -> fmt::Result {
    counted(f, label, rows.len() as u64)?;
    rows.try_for_each(|row| writeln!(f, "{row}"))
}

fn describe(reason: &SkipReason) -> String {
    match reason {
        SkipReason::Name { value } => format!("unrepresentable name {:?}", value.as_str()),
        SkipReason::Rkey { value } => format!("unrepresentable rkey {:?}", value.as_str()),
        SkipReason::RkeyCollision { rkey, winner } => {
            format!(
                "record key {:?} belongs to the alias-backed {winner}",
                rkey.as_str()
            )
        }
        SkipReason::NoSourceRepo => "no git repository at the source path".to_string(),
        SkipReason::UnreadableSource { kind } => {
            format!("a source path that this process can't read: {kind}")
        }
    }
}
