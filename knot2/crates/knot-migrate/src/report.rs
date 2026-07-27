use std::fmt::{self, Display, Formatter};

use crate::adopt::AdoptOutcome;
use crate::emit::CobSummary;
use crate::mapping::{Mapping, SkipReason};

pub struct Report<'a> {
    pub mapping: &'a Mapping,
    pub orphan_alias_count: u64,
    pub adoption: Option<&'a AdoptOutcome>,
    pub cobs: Option<&'a CobSummary>,
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
        writeln!(f, "casbin cross-check drift:")?;
        writeln!(
            f,
            "acl-only collaborator grants unioned in: {}",
            drift.acl_only_collaborators.len()
        )?;
        drift
            .acl_only_collaborators
            .iter()
            .try_for_each(|(repo, did)| writeln!(f, "{repo} <- {did}"))?;
        writeln!(
            f,
            "table-only collaborator grants missing from acl: {}",
            drift.table_only_collaborators.len()
        )?;
        drift
            .table_only_collaborators
            .iter()
            .try_for_each(|(repo, did)| writeln!(f, "{repo} <- {did}"))?;
        writeln!(
            f,
            "repos with no acl owner marker where the owner regains push: {}",
            drift.markerless_owner_repos.len()
        )?;
        drift
            .markerless_owner_repos
            .iter()
            .try_for_each(|repo| writeln!(f, "{repo}"))?;
        writeln!(
            f,
            "orphan owner markers on unknown repos: {}",
            drift.orphan_owner_markers.len()
        )?;
        writeln!(
            f,
            "extra acl owner markers dropped: {}",
            drift.extra_owner_markers.len()
        )?;
        drift
            .extra_owner_markers
            .iter()
            .try_for_each(|(repo, did)| writeln!(f, "{repo} <- {did}"))?;
        writeln!(
            f,
            "orphan collaborator pairs on unknown repos: {}",
            drift.orphan_collaborator_pairs.len()
        )?;
        writeln!(
            f,
            "acl-only members unioned in: {}",
            drift.acl_only_members.len()
        )?;
        drift
            .acl_only_members
            .iter()
            .try_for_each(|did| writeln!(f, "{did}"))?;
        writeln!(
            f,
            "table-only members missing from acl: {}",
            drift.table_only_members.len()
        )?;
        writeln!(f, "slash-form owner markers: {}", drift.slash_owner_markers)?;
        writeln!(
            f,
            "slash-form collaborator rows: {}",
            drift.slash_collab_rows
        )?;
        writeln!(
            f,
            "slash-resolved collaborator grants left out of the union: {}",
            drift.slash_resolved_collaborators.len()
        )?;
        drift
            .slash_resolved_collaborators
            .iter()
            .try_for_each(|(repo, did)| writeln!(f, "{repo} <- {did}"))?;
        writeln!(
            f,
            "unresolved slash forms: {}",
            drift.unresolved_slash_forms.len()
        )?;
        drift
            .unresolved_slash_forms
            .iter()
            .try_for_each(|form| writeln!(f, "{form}"))?;
        writeln!(f, "orphan aliases: {}", self.orphan_alias_count)?;
        writeln!(f)?;
        writeln!(f, "skipped repos: {}", mapping.skipped.len())?;
        mapping.skipped.iter().try_for_each(|skip| {
            writeln!(f, "{} {}", skip.repo_did, describe(&skip.reason))?;
            skip.lost_collaborators
                .iter()
                .try_for_each(|did| writeln!(f, "drops collaborator grant for {did}"))
        })?;
        self.adoption.map_or(Ok(()), |adoption| {
            writeln!(f)?;
            writeln!(
                f,
                "adopted by {}: {} new, {} already present, {} sha1, {} sha256",
                adoption.transfer,
                adoption.adopted,
                adoption.already_present,
                adoption.sha1,
                adoption.sha256
            )
        })?;
        self.cobs.map_or(Ok(()), |cobs| {
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
        })
    }
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
    }
}
