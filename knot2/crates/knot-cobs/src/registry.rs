use std::collections::BTreeMap;

use knot_cob::{
    ChangeId, ChangePayload, Checkpoint, CobError, CobHome, CobId, CobStore, Evaluate,
    HistoryModel, SnapshotStride, StateSize,
};
use knot_runtime::Signer;
use knot_types::{ActorId, OwnerDid, RepoDid, RepoName, RepoRkey, UnixSeconds};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    pub owner: OwnerDid,
    pub rkey: RepoRkey,
    pub name: RepoName,
    pub repo: RepoDid,
    pub created_at: UnixSeconds,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rename {
    pub owner: OwnerDid,
    pub rkey: RepoRkey,
    pub name: RepoName,
    pub repo: RepoDid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRef {
    pub owner: OwnerDid,
    pub rkey: RepoRkey,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", content = "data", rename_all = "snake_case")]
pub enum RegistryChange {
    Register(Registration),
    Rename(Rename),
    Deregister(RepoRef),
}

impl ChangePayload for RegistryChange {
    const TYPE: &'static str = "sh.tangled.knot.repoRegistry";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRecord {
    pub owner: OwnerDid,
    pub rkey: RepoRkey,
    pub name: RepoName,
    pub created_at: UnixSeconds,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Registry {
    records: BTreeMap<RepoDid, RepoRecord>,
    aliases: BTreeMap<OwnerDid, BTreeMap<RepoRkey, RepoDid>>,
}

impl Registry {
    pub fn resolve(&self, owner: &OwnerDid, rkey: &RepoRkey) -> Option<&RepoDid> {
        self.aliases.get(owner)?.get(rkey)
    }

    pub fn record_of(&self, repo: &RepoDid) -> Option<&RepoRecord> {
        self.records.get(repo)
    }

    pub fn owner_of(&self, repo: &RepoDid) -> Option<OwnerDid> {
        self.records.get(repo).map(|record| record.owner.clone())
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn records(&self) -> impl Iterator<Item = (&RepoDid, &RepoRecord)> {
        self.records.iter()
    }

    pub fn aliases(&self) -> impl Iterator<Item = (&OwnerDid, &RepoRkey, &RepoDid)> {
        self.aliases
            .iter()
            .flat_map(|(owner, names)| names.iter().map(move |(rkey, repo)| (owner, rkey, repo)))
    }

    fn canonical_holder(&self, owner: &OwnerDid, rkey: &RepoRkey) -> Option<&RepoDid> {
        let holder = self.resolve(owner, rkey)?;
        self.records
            .get(holder)
            .filter(|record| record.rkey == *rkey)
            .map(|_| holder)
    }

    fn register(mut self, registration: Registration) -> Self {
        self = self.drop_repo(&registration.repo);
        self = self.steal_alias(&registration.owner, &registration.rkey, &registration.repo);
        self.aliases
            .entry(registration.owner.clone())
            .or_default()
            .insert(registration.rkey.clone(), registration.repo.clone());
        self.records.insert(
            registration.repo,
            RepoRecord {
                owner: registration.owner,
                rkey: registration.rkey,
                name: registration.name,
                created_at: registration.created_at,
            },
        );
        self
    }

    fn rename(mut self, rename: Rename) -> Self {
        match self.records.get(&rename.repo) {
            Some(record) if record.owner == rename.owner => {}
            _ => return self,
        }
        self = self.steal_alias(&rename.owner, &rename.rkey, &rename.repo);
        self.aliases
            .entry(rename.owner.clone())
            .or_default()
            .insert(rename.rkey.clone(), rename.repo.clone());
        if let Some(record) = self.records.get_mut(&rename.repo) {
            record.rkey = rename.rkey;
            record.name = rename.name;
        }
        self
    }

    fn deregister(self, target: RepoRef) -> Self {
        match self.resolve(&target.owner, &target.rkey).cloned() {
            Some(repo) => self.drop_repo(&repo),
            None => self,
        }
    }

    fn steal_alias(mut self, owner: &OwnerDid, rkey: &RepoRkey, target: &RepoDid) -> Self {
        match self.resolve(owner, rkey).cloned() {
            Some(holder) if holder != *target => {
                let canonical = self
                    .records
                    .get(&holder)
                    .is_some_and(|record| record.rkey == *rkey);
                if canonical {
                    self.drop_repo(&holder)
                } else {
                    if let Some(names) = self.aliases.get_mut(owner) {
                        names.remove(rkey);
                    }
                    self.prune_empty_owners()
                }
            }
            _ => self,
        }
    }

    fn drop_repo(mut self, repo: &RepoDid) -> Self {
        self.records.remove(repo);
        self.aliases
            .values_mut()
            .for_each(|names| names.retain(|_, holder| holder != repo));
        self.prune_empty_owners()
    }

    fn prune_empty_owners(mut self) -> Self {
        self.aliases.retain(|_, names| !names.is_empty());
        self
    }
}

pub struct RepoRegistryCob;

impl Evaluate for RepoRegistryCob {
    type State = Registry;
    type Change = RegistryChange;

    const HISTORY: HistoryModel = HistoryModel::Linear;

    fn initial() -> Self::State {
        Registry::default()
    }

    fn apply(state: Self::State, change: Self::Change, _author: &ActorId) -> Self::State {
        match change {
            RegistryChange::Register(registration) => state.register(registration),
            RegistryChange::Rename(rename) => state.rename(rename),
            RegistryChange::Deregister(target) => state.deregister(target),
        }
    }
}

impl Checkpoint for RepoRegistryCob {
    const SNAPSHOT_STRIDE: SnapshotStride = SnapshotStride::new(256);
    fn checkpoint_size(state: &Self::State) -> StateSize {
        StateSize::new(state.len())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error(transparent)]
    Cob(#[from] CobError),
    #[error("no repo is registered at {owner}/{rkey}")]
    NotRegistered { owner: OwnerDid, rkey: RepoRkey },
    #[error("record key {owner}/{rkey} resolves to {found}, expected {expected}")]
    RepoMismatch {
        owner: OwnerDid,
        rkey: RepoRkey,
        expected: RepoDid,
        found: RepoDid,
    },
    #[error("repo {repo} is already registered as {owner}/{rkey}")]
    AlreadyRegistered {
        repo: RepoDid,
        owner: OwnerDid,
        rkey: RepoRkey,
    },
    #[error("record key {owner}/{rkey} is canonical key of {existing}")]
    RkeyTaken {
        owner: OwnerDid,
        rkey: RepoRkey,
        existing: RepoDid,
    },
    #[error("repo {repo} isn't hosted on this knot")]
    NotHosted { repo: RepoDid },
    #[error("repo {repo} is no longer registered to {expected}")]
    OwnerMoved { repo: RepoDid, expected: OwnerDid },
}

pub fn register_repo(
    store: &CobStore,
    home: &CobHome,
    object: CobId,
    registration: Registration,
    signer: &dyn Signer,
    timestamp: UnixSeconds,
) -> Result<Option<ChangeId>, RegistryError> {
    store.update_maybe_checkpointed::<RepoRegistryCob, RegistryError>(
        home,
        object,
        signer,
        timestamp,
        |registry| {
            if let Some(holder) = registry.canonical_holder(&registration.owner, &registration.rkey)
                && holder != &registration.repo
            {
                return Err(RegistryError::RkeyTaken {
                    owner: registration.owner.clone(),
                    rkey: registration.rkey.clone(),
                    existing: holder.clone(),
                });
            }
            match registry.record_of(&registration.repo) {
                Some(record)
                    if record.owner != registration.owner || record.rkey != registration.rkey =>
                {
                    Err(RegistryError::AlreadyRegistered {
                        repo: registration.repo.clone(),
                        owner: record.owner.clone(),
                        rkey: record.rkey.clone(),
                    })
                }
                Some(_) => Ok(None),
                None => Ok(Some(RegistryChange::Register(registration.clone()))),
            }
        },
    )
}

pub fn rename_repo(
    store: &CobStore,
    home: &CobHome,
    object: CobId,
    rename: Rename,
    signer: &dyn Signer,
    timestamp: UnixSeconds,
) -> Result<Option<ChangeId>, RegistryError> {
    store.update_maybe_checkpointed::<RepoRegistryCob, RegistryError>(
        home,
        object,
        signer,
        timestamp,
        |registry| {
            let record =
                registry
                    .record_of(&rename.repo)
                    .ok_or_else(|| RegistryError::NotHosted {
                        repo: rename.repo.clone(),
                    })?;
            if record.owner != rename.owner {
                return Err(RegistryError::OwnerMoved {
                    repo: rename.repo.clone(),
                    expected: rename.owner.clone(),
                });
            }
            if record.rkey == rename.rkey && record.name == rename.name {
                return Ok(None);
            }
            if let Some(holder) = registry.canonical_holder(&rename.owner, &rename.rkey)
                && holder != &rename.repo
            {
                return Err(RegistryError::RkeyTaken {
                    owner: rename.owner.clone(),
                    rkey: rename.rkey.clone(),
                    existing: holder.clone(),
                });
            }
            Ok(Some(RegistryChange::Rename(rename.clone())))
        },
    )
}

pub fn deregister_repo(
    store: &CobStore,
    home: &CobHome,
    object: CobId,
    target: RepoRef,
    expected: RepoDid,
    signer: &dyn Signer,
    timestamp: UnixSeconds,
) -> Result<ChangeId, RegistryError> {
    store.update_with_checkpointed::<RepoRegistryCob, RegistryError>(
        home,
        object,
        signer,
        timestamp,
        |registry| match registry.resolve(&target.owner, &target.rkey) {
            None => Err(RegistryError::NotRegistered {
                owner: target.owner.clone(),
                rkey: target.rkey.clone(),
            }),
            Some(found) if found != &expected => Err(RegistryError::RepoMismatch {
                owner: target.owner.clone(),
                rkey: target.rkey.clone(),
                expected: expected.clone(),
                found: found.clone(),
            }),
            Some(_) => Ok(RegistryChange::Deregister(target.clone())),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(suffix: &str) -> OwnerDid {
        OwnerDid::new(format!("did:plc:{suffix}")).unwrap()
    }

    fn repo(suffix: &str) -> RepoDid {
        RepoDid::new(format!("did:plc:{suffix}")).unwrap()
    }

    fn rkey(value: &str) -> RepoRkey {
        RepoRkey::new(value).unwrap()
    }

    fn name(value: &str) -> RepoName {
        RepoName::new(value).unwrap()
    }

    fn register(owner_id: &str, key: &str, repo_id: &str, at: i64) -> RegistryChange {
        RegistryChange::Register(Registration {
            owner: owner(owner_id),
            rkey: rkey(key),
            name: name(key),
            repo: repo(repo_id),
            created_at: UnixSeconds::new(at),
        })
    }

    fn rename(owner_id: &str, key: &str, repo_id: &str) -> RegistryChange {
        RegistryChange::Rename(Rename {
            owner: owner(owner_id),
            rkey: rkey(key),
            name: name(key),
            repo: repo(repo_id),
        })
    }

    fn deregister(owner_id: &str, key: &str) -> RegistryChange {
        RegistryChange::Deregister(RepoRef {
            owner: owner(owner_id),
            rkey: rkey(key),
        })
    }

    fn fold(changes: Vec<RegistryChange>) -> Registry {
        let author = ActorId::from_secp256k1(&[0x02; 33]);
        changes
            .into_iter()
            .fold(RepoRegistryCob::initial(), |state, change| {
                RepoRegistryCob::apply(state, change, &author)
            })
    }

    #[test]
    fn register_maps_owner_and_rkey_to_a_repo() {
        let state = fold(vec![register("nel", "anemone", "squid", 5)]);
        assert_eq!(
            state.resolve(&owner("nel"), &rkey("anemone")),
            Some(&repo("squid"))
        );
        let record = state.record_of(&repo("squid")).unwrap();
        assert_eq!(record.owner, owner("nel"));
        assert_eq!(record.rkey, rkey("anemone"));
        assert_eq!(record.name, name("anemone"));
        assert_eq!(record.created_at, UnixSeconds::new(5));
    }

    #[test]
    fn re_register_replaces_the_repo_under_an_rkey() {
        let state = fold(vec![
            register("nel", "anemone", "squid", 1),
            register("nel", "anemone", "limpet", 2),
        ]);
        assert_eq!(
            state.resolve(&owner("nel"), &rkey("anemone")),
            Some(&repo("limpet"))
        );
        assert!(
            state.record_of(&repo("squid")).is_none(),
            "repo whose canonical rkey is taken by later register is dropped wholesale"
        );
        assert_eq!(state.len(), 1);
    }

    #[test]
    fn rename_retains_the_prior_rkey_as_an_alias() {
        let state = fold(vec![
            register("nel", "anemone", "squid", 1),
            rename("nel", "barnacle", "squid"),
        ]);
        assert_eq!(
            state.resolve(&owner("nel"), &rkey("barnacle")),
            Some(&repo("squid")),
            "new rkey resolves"
        );
        assert_eq!(
            state.resolve(&owner("nel"), &rkey("anemone")),
            Some(&repo("squid")),
            "prior rkey keeps resolving as an alias"
        );
        let record = state.record_of(&repo("squid")).unwrap();
        assert_eq!(record.rkey, rkey("barnacle"));
        assert_eq!(record.name, name("barnacle"));
        assert_eq!(state.len(), 1);
    }

    #[test]
    fn rename_of_an_unregistered_repo_is_a_no_op() {
        let registered = fold(vec![register("nel", "anemone", "squid", 1)]);
        let after = fold(vec![
            register("nel", "anemone", "squid", 1),
            rename("nel", "barnacle", "conch"),
        ]);
        assert_eq!(after, registered);
    }

    #[test]
    fn rename_under_a_mismatched_owner_is_a_no_op() {
        let registered = fold(vec![register("nel", "anemone", "squid", 1)]);
        let after = fold(vec![
            register("nel", "anemone", "squid", 1),
            rename("olaren", "barnacle", "squid"),
        ]);
        assert_eq!(after, registered);
    }

    #[test]
    fn deregister_by_any_alias_removes_the_repo_and_every_alias() {
        let state = fold(vec![
            register("nel", "anemone", "squid", 1),
            rename("nel", "barnacle", "squid"),
            deregister("nel", "anemone"),
            deregister("nel", "anemone"),
        ]);
        assert_eq!(state.resolve(&owner("nel"), &rkey("anemone")), None);
        assert_eq!(state.resolve(&owner("nel"), &rkey("barnacle")), None);
        assert!(state.is_empty(), "replaying a deregister folds as a no-op");
        assert_eq!(state, Registry::default());
    }

    #[test]
    fn a_later_change_steals_a_stale_alias_but_keeps_the_victim_canonical() {
        let state = fold(vec![
            register("nel", "anemone", "squid", 1),
            rename("nel", "barnacle", "squid"),
            register("nel", "anemone", "whelk", 2),
        ]);
        assert_eq!(
            state.resolve(&owner("nel"), &rkey("anemone")),
            Some(&repo("whelk")),
            "later register wins stale alias"
        );
        assert_eq!(
            state.resolve(&owner("nel"), &rkey("barnacle")),
            Some(&repo("squid")),
            "victim keeps its canonical rkey"
        );
        assert_eq!(state.len(), 2);
    }

    #[test]
    fn owner_of_resolves_through_the_record_with_later_register_precedence() {
        let unique = fold(vec![
            register("nel", "anemone", "squid", 1),
            register("nel", "barnacle", "whelk", 2),
        ]);
        assert_eq!(unique.owner_of(&repo("squid")), Some(owner("nel")));
        assert_eq!(
            unique.record_of(&repo("squid")).unwrap().rkey,
            rkey("anemone")
        );
        assert_eq!(unique.owner_of(&repo("conch")), None);

        let moved = fold(vec![
            register("nel", "anemone", "squid", 1),
            register("olaren", "fork", "squid", 2),
        ]);
        assert_eq!(
            moved.owner_of(&repo("squid")),
            Some(owner("olaren")),
            "linear causal order gives later register deterministic precedence"
        );
        assert_eq!(
            moved.resolve(&owner("nel"), &rkey("anemone")),
            None,
            "re-register under a new owner drops old owner's aliases"
        );
    }

    #[test]
    fn change_payload_roundtrips_through_dag_cbor() {
        [
            register("nel", "anemone", "squid", 5),
            rename("nel", "barnacle", "squid"),
            deregister("nel", "anemone"),
        ]
        .into_iter()
        .for_each(|change| {
            let bytes = change.encode().unwrap();
            assert_eq!(RegistryChange::decode(&bytes).unwrap(), change);
        });
    }
}
