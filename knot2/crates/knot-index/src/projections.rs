use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use knot_cob::{Change, ChangeId, ChangePayload, Checkpoint, CobId, CobStore, Evaluate};
use knot_cobs::{
    CollaboratorsChange, CollaboratorsCob, Grant, GrantChange, Registration, Registry,
    RegistryChange, Rename, RepoRef, RepoRegistryCob, Roster,
};
use knot_types::{AccountDid, ClonePath, OfferedKey, OwnerDid, RepoDid, RepoRkey, UnixSeconds};

use crate::coverage::{Coverage, CoverageCell, Resolved};
use crate::error::IndexError;
use crate::intern::{AccountKey, Interner, NameKey, OwnerKey, RepoKey, RkeyKey};
use crate::{
    IndexGeneration, KeyBudget, KeyLease, KeyRecord, KeyReprieve, KeyReprieved, SweepFloor,
};

#[derive(Debug, Clone, Copy)]
struct Provenance {
    added_by: AccountKey,
    created_at: UnixSeconds,
}

impl Provenance {
    fn intern(interner: &Interner, grant: &Grant) -> Self {
        Self {
            added_by: interner.intern_account(&grant.added_by),
            created_at: grant.created_at,
        }
    }

    fn grant(self, interner: &Interner, subject: AccountKey) -> Grant {
        Grant {
            subject: interner.resolve_account(subject),
            added_by: interner.resolve_account(self.added_by),
            created_at: self.created_at,
        }
    }
}

fn decode_change<P: ChangePayload>(change: &Change) -> Result<P, IndexError> {
    if change.type_name != P::type_name() {
        return Err(IndexError::UnexpectedType {
            change: change.id,
            expected: P::type_name(),
            found: change.type_name.clone(),
        });
    }
    P::decode(change.payload()).map_err(|error| IndexError::Decode {
        change: change.id,
        type_name: P::type_name(),
        reason: error.to_string(),
    })
}

fn decode_delta<P: ChangePayload>(changes: &[Change]) -> Result<Vec<P>, IndexError> {
    changes.iter().map(decode_change::<P>).collect()
}

fn group_by<K: Ord, T>(items: Vec<T>, key: impl Fn(&T) -> K) -> BTreeMap<K, Vec<T>> {
    items.into_iter().fold(BTreeMap::new(), |mut acc, item| {
        acc.entry(key(&item)).or_default().push(item);
        acc
    })
}

pub(crate) struct GrantSetProjection<Cob>
where
    Cob: Evaluate,
    Cob::Change: ChangePayload + GrantChange,
{
    membership: scc::HashMap<AccountKey, Provenance>,
    coverage: CoverageCell,
    tip: Mutex<Option<ChangeId>>,
    _cob: PhantomData<fn() -> Cob>,
}

impl<Cob> GrantSetProjection<Cob>
where
    Cob: Evaluate,
    Cob::Change: ChangePayload + GrantChange,
{
    pub(crate) fn new() -> Self {
        Self {
            membership: scc::HashMap::new(),
            coverage: CoverageCell::new(Coverage::Warming),
            tip: Mutex::new(None),
            _cob: PhantomData,
        }
    }

    pub(crate) fn coverage(&self) -> Coverage {
        self.coverage.get()
    }

    pub(crate) fn reset(&self) {
        let mut tip = self
            .tip
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.membership.clear_sync();
        *tip = None;
        self.coverage.set(Coverage::Ready);
    }

    pub(crate) fn contains(&self, interner: &Interner, did: &AccountDid) -> Resolved<bool> {
        match self.coverage.get() {
            Coverage::Warming => Resolved::Warming,
            Coverage::Ready => Resolved::Ready(
                interner
                    .account(did)
                    .is_some_and(|did| self.membership.contains_sync(&did)),
            ),
        }
    }

    pub(crate) fn entries(&self, interner: &Interner) -> Resolved<Vec<Grant>> {
        match self.coverage.get() {
            Coverage::Warming => Resolved::Warming,
            Coverage::Ready => {
                let mut out = BTreeMap::new();
                self.membership.iter_sync(|&subject, slot| {
                    let grant = slot.grant(interner, subject);
                    out.insert(grant.subject.clone(), grant);
                    true
                });
                Resolved::Ready(out.into_values().collect())
            }
        }
    }

    pub(crate) fn refresh(
        &self,
        interner: &Interner,
        store: &CobStore,
        object: CobId,
    ) -> Result<(), IndexError>
    where
        Cob: Checkpoint + Evaluate<State = Roster>,
    {
        let mut tip = self
            .tip
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match *tip {
            None => {
                let (roster, seeded) = store
                    .materialize::<Cob>(object)
                    .inspect_err(|_| self.coverage.set(Coverage::Warming))?;
                self.seed(interner, &roster);
                *tip = Some(seeded);
            }
            Some(prev) => {
                let delta = store
                    .changes_since::<Cob>(object, Some(prev))
                    .inspect_err(|_| self.coverage.set(Coverage::Warming))?;
                let decoded = decode_delta::<Cob::Change>(&delta.changes)
                    .inspect_err(|_| self.coverage.set(Coverage::Warming))?;
                self.apply_delta(interner, decoded);
                *tip = Some(delta.tip);
            }
        }
        self.coverage.set(Coverage::Ready);
        Ok(())
    }

    fn seed(&self, interner: &Interner, roster: &Roster) {
        self.membership.clear_sync();
        roster.entries().for_each(|(subject, entry)| {
            let slot = Provenance {
                added_by: interner.intern_account(&entry.added_by),
                created_at: entry.created_at,
            };
            let _ = self
                .membership
                .insert_sync(interner.intern_account(subject), slot);
        });
    }

    fn apply_delta(&self, interner: &Interner, changes: Vec<Cob::Change>) {
        group_by(changes, |change| change.subject().clone())
            .into_iter()
            .for_each(|(did, ops)| {
                let current = interner
                    .account(&did)
                    .and_then(|key| self.membership.read_sync(&key, |_, slot| *slot));
                let net = ops
                    .into_iter()
                    .fold(current, |slot, change| match change.as_grant() {
                        Some(grant) => slot.or_else(|| Some(Provenance::intern(interner, grant))),
                        None => None,
                    });
                match net {
                    Some(slot) => {
                        *self
                            .membership
                            .entry_sync(interner.intern_account(&did))
                            .or_insert(slot)
                            .get_mut() = slot;
                    }
                    None => {
                        if let Some(key) = interner.account(&did) {
                            let _ = self.membership.remove_sync(&key);
                        }
                    }
                }
            });
    }
}

struct RepoRoster {
    tip: Option<ChangeId>,
    entries: BTreeMap<AccountKey, Provenance>,
}

const REPO_LOCK_STRIPES: usize = 256;

pub(crate) struct CollaboratorsProjection {
    rosters: scc::HashMap<RepoKey, RepoRoster>,
    locks: [Mutex<()>; REPO_LOCK_STRIPES],
}

impl CollaboratorsProjection {
    pub(crate) fn new() -> Self {
        Self {
            rosters: scc::HashMap::new(),
            locks: std::array::from_fn(|_| Mutex::new(())),
        }
    }

    fn repo_lock(&self, repo: RepoKey) -> &Mutex<()> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        repo.hash(&mut hasher);
        &self.locks[(hasher.finish() % REPO_LOCK_STRIPES as u64) as usize]
    }

    pub(crate) fn coverage(&self) -> Coverage {
        Coverage::Ready
    }

    pub(crate) fn is_folded(&self, interner: &Interner, repo: &RepoDid) -> bool {
        interner
            .repo(repo)
            .is_some_and(|repo| self.rosters.contains_sync(&repo))
    }

    pub(crate) fn contains(
        &self,
        interner: &Interner,
        repo: &RepoDid,
        did: &AccountDid,
    ) -> Resolved<bool> {
        let Some(repo) = interner.repo(repo) else {
            return Resolved::Warming;
        };
        match self.rosters.read_sync(&repo, |_, roster| {
            interner
                .account(did)
                .is_some_and(|account| roster.entries.contains_key(&account))
        }) {
            Some(present) => Resolved::Ready(present),
            None => Resolved::Warming,
        }
    }

    pub(crate) fn entries(&self, interner: &Interner, repo: &RepoDid) -> Resolved<Vec<Grant>> {
        let Some(repo) = interner.repo(repo) else {
            return Resolved::Warming;
        };
        match self
            .rosters
            .read_sync(&repo, |_, roster| roster_grants(interner, roster))
        {
            Some(grants) => Resolved::Ready(grants),
            None => Resolved::Warming,
        }
    }

    pub(crate) fn mark_repo_empty(&self, repo: RepoKey) {
        let lock = self.repo_lock(repo);
        let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        self.install(
            repo,
            RepoRoster {
                tip: None,
                entries: BTreeMap::new(),
            },
        );
    }

    pub(crate) fn refresh_repo(
        &self,
        interner: &Interner,
        store: &CobStore,
        repo: RepoKey,
        object: CobId,
    ) -> Result<(), IndexError> {
        let lock = self.repo_lock(repo);
        let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        self.fold_repo(interner, store, repo, object)
            .inspect_err(|_| self.purge_repo(repo))
    }

    fn fold_repo(
        &self,
        interner: &Interner,
        store: &CobStore,
        repo: RepoKey,
        object: CobId,
    ) -> Result<(), IndexError> {
        let prev = self
            .rosters
            .read_sync(&repo, |_, roster| roster.tip)
            .flatten();
        match prev {
            None => {
                let (roster, tip) = store.materialize::<CollaboratorsCob>(object)?;
                let entries = roster
                    .entries()
                    .map(|(subject, entry)| {
                        (
                            interner.intern_account(subject),
                            Provenance {
                                added_by: interner.intern_account(&entry.added_by),
                                created_at: entry.created_at,
                            },
                        )
                    })
                    .collect();
                self.install(
                    repo,
                    RepoRoster {
                        tip: Some(tip),
                        entries,
                    },
                );
            }
            Some(prev) => {
                let delta = store.changes_since::<CollaboratorsCob>(object, Some(prev))?;
                let decoded = decode_delta::<CollaboratorsChange>(&delta.changes)?;
                let mut occupied = self.rosters.entry_sync(repo).or_insert_with(|| RepoRoster {
                    tip: None,
                    entries: BTreeMap::new(),
                });
                let roster = occupied.get_mut();
                roster.entries = decoded
                    .into_iter()
                    .fold(std::mem::take(&mut roster.entries), |entries, change| {
                        apply(interner, entries, change)
                    });
                roster.tip = Some(delta.tip);
            }
        }
        Ok(())
    }

    pub(crate) fn drop_repo(&self, repo: RepoKey) {
        let lock = self.repo_lock(repo);
        let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        self.purge_repo(repo);
    }

    fn purge_repo(&self, repo: RepoKey) {
        let _ = self.rosters.remove_sync(&repo);
    }

    fn install(&self, repo: RepoKey, roster: RepoRoster) {
        match self.rosters.entry_sync(repo) {
            scc::hash_map::Entry::Occupied(mut occupied) => {
                let _ = occupied.insert(roster);
            }
            scc::hash_map::Entry::Vacant(vacant) => {
                vacant.insert_entry(roster);
            }
        }
    }
}

fn roster_grants(interner: &Interner, roster: &RepoRoster) -> Vec<Grant> {
    roster
        .entries
        .iter()
        .map(|(&subject, slot)| {
            let grant = slot.grant(interner, subject);
            (grant.subject.clone(), grant)
        })
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect()
}

fn apply(
    interner: &Interner,
    mut entries: BTreeMap<AccountKey, Provenance>,
    change: CollaboratorsChange,
) -> BTreeMap<AccountKey, Provenance> {
    match change {
        CollaboratorsChange::Add(grant) => {
            entries
                .entry(interner.intern_account(&grant.subject))
                .or_insert_with(|| Provenance::intern(interner, &grant));
        }
        CollaboratorsChange::Remove(removal) => {
            if let Some(key) = interner.account(&removal.subject) {
                entries.remove(&key);
            }
        }
    }
    entries
}

struct RecordSlot {
    owner: OwnerKey,
    rkey: RkeyKey,
    name: NameKey,
    created_at: UnixSeconds,
}

pub(crate) struct RegistryProjection {
    aliases: scc::HashMap<(OwnerKey, RkeyKey), RepoKey>,
    names: scc::HashMap<(OwnerKey, NameKey), BTreeSet<(UnixSeconds, RepoKey)>>,
    records: scc::HashMap<RepoKey, RecordSlot>,
    coverage: CoverageCell,
    tip: Mutex<Option<ChangeId>>,
}

impl RegistryProjection {
    pub(crate) fn new() -> Self {
        Self {
            aliases: scc::HashMap::new(),
            names: scc::HashMap::new(),
            records: scc::HashMap::new(),
            coverage: CoverageCell::new(Coverage::Warming),
            tip: Mutex::new(None),
        }
    }

    pub(crate) fn coverage(&self) -> Coverage {
        self.coverage.get()
    }

    pub(crate) fn reset(&self, interner: &Interner) -> Vec<RepoDid> {
        let mut tip = self
            .tip
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let evacuated = self.hosted_repos(interner);
        self.aliases.clear_sync();
        self.names.clear_sync();
        self.records.clear_sync();
        *tip = None;
        self.coverage.set(Coverage::Ready);
        evacuated
    }

    pub(crate) fn resolve(
        &self,
        interner: &Interner,
        owner: &OwnerDid,
        rkey: &RepoRkey,
    ) -> Resolved<Option<RepoDid>> {
        match self.coverage.get() {
            Coverage::Warming => Resolved::Warming,
            Coverage::Ready => Resolved::Ready(
                interner
                    .owner(owner)
                    .zip(interner.rkey(rkey))
                    .and_then(|key| self.aliases.read_sync(&key, |_, repo| *repo))
                    .map(|repo| interner.resolve_repo(repo)),
            ),
        }
    }

    pub(crate) fn resolve_clone_path(
        &self,
        interner: &Interner,
        owner: &OwnerDid,
        path: &ClonePath,
    ) -> Resolved<Option<RepoDid>> {
        if self.coverage.get() == Coverage::Warming {
            return Resolved::Warming;
        }
        let Some(owner) = interner.owner(owner) else {
            return Resolved::Ready(None);
        };
        let by_rkey = path
            .rkeys()
            .filter_map(|rkey| interner.rkey(rkey))
            .find_map(|rkey| self.aliases.read_sync(&(owner, rkey), |_, repo| *repo));
        if let Some(repo) = by_rkey {
            return Resolved::Ready(Some(interner.resolve_repo(repo)));
        }
        Resolved::Ready(
            path.names()
                .filter_map(|name| interner.name(name))
                .find_map(|name| self.oldest_registration_for_name(interner, owner, name)),
        )
    }

    fn oldest_registration_for_name(
        &self,
        interner: &Interner,
        owner: OwnerKey,
        name: NameKey,
    ) -> Option<RepoDid> {
        // Notice how set keys sort by whichever string the interner saw first,
        // and a cold rebuild will see them in a different order than live replay,
        // so 2 entries with the same timestamp will settle by comparing
        // DIDs instead.
        self.names
            .read_sync(&(owner, name), |_, registered| {
                let earliest = registered.first()?.0;
                registered
                    .iter()
                    .take_while(|(created_at, _)| *created_at == earliest)
                    .map(|(_, repo)| interner.resolve_repo(*repo))
                    .min()
            })
            .flatten()
    }

    pub(crate) fn owner_of(
        &self,
        interner: &Interner,
        repo: &RepoDid,
    ) -> Resolved<Option<OwnerDid>> {
        if self.coverage.get() == Coverage::Warming {
            return Resolved::Warming;
        }
        let Some(target) = interner.repo(repo) else {
            return Resolved::Ready(None);
        };
        Resolved::Ready(
            self.records
                .read_sync(&target, |_, slot| slot.owner)
                .map(|owner| interner.resolve_owner(owner)),
        )
    }

    pub(crate) fn rkey_of(
        &self,
        interner: &Interner,
        repo: &RepoDid,
    ) -> Resolved<Option<RepoRkey>> {
        if self.coverage.get() == Coverage::Warming {
            return Resolved::Warming;
        }
        let Some(target) = interner.repo(repo) else {
            return Resolved::Ready(None);
        };
        Resolved::Ready(
            self.records
                .read_sync(&target, |_, slot| slot.rkey)
                .map(|rkey| interner.resolve_rkey(rkey)),
        )
    }

    pub(crate) fn hosted_repos(&self, interner: &Interner) -> Vec<RepoDid> {
        let mut repos = BTreeSet::new();
        self.records.iter_sync(|repo, _| {
            repos.insert(interner.resolve_repo(*repo));
            true
        });
        repos.into_iter().collect()
    }

    pub(crate) fn refresh(
        &self,
        interner: &Interner,
        store: &CobStore,
        object: CobId,
    ) -> Result<Vec<RepoDid>, IndexError> {
        let mut tip = self
            .tip
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match *tip {
            None => {
                let (registry, seeded) = store
                    .materialize::<RepoRegistryCob>(object)
                    .inspect_err(|_| self.coverage.set(Coverage::Warming))?;
                self.seed(interner, &registry);
                *tip = Some(seeded);
                self.coverage.set(Coverage::Ready);
                Ok(Vec::new())
            }
            Some(prev) => {
                let delta = store
                    .changes_since::<RepoRegistryCob>(object, Some(prev))
                    .inspect_err(|_| self.coverage.set(Coverage::Warming))?;
                let decoded = decode_delta::<RegistryChange>(&delta.changes)
                    .inspect_err(|_| self.coverage.set(Coverage::Warming))?;
                let displaced = self.apply_delta(interner, decoded);
                *tip = Some(delta.tip);
                self.coverage.set(Coverage::Ready);
                Ok(self.evacuated(interner, displaced))
            }
        }
    }

    fn seed(&self, interner: &Interner, registry: &Registry) {
        self.aliases.clear_sync();
        self.names.clear_sync();
        self.records.clear_sync();
        registry.records().for_each(|(repo, record)| {
            let repo = interner.intern_repo(repo);
            let owner = interner.intern_owner(&record.owner);
            let name = interner.intern_name(&record.name);
            self.upsert_record(
                repo,
                owner,
                interner.intern_rkey(&record.rkey),
                name,
                record.created_at,
            );
            self.bind_name(owner, name, record.created_at, repo);
        });
        registry.aliases().for_each(|(owner, rkey, repo)| {
            self.upsert_alias(
                interner.intern_owner(owner),
                interner.intern_rkey(rkey),
                interner.intern_repo(repo),
            );
        });
    }

    fn evacuated(&self, interner: &Interner, displaced: Vec<RepoKey>) -> Vec<RepoDid> {
        if displaced.is_empty() {
            return Vec::new();
        }
        let live = self.live_repos();
        displaced
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|repo| !live.contains(repo))
            .map(|repo| interner.resolve_repo(repo))
            .collect()
    }

    fn live_repos(&self) -> BTreeSet<RepoKey> {
        let mut live = BTreeSet::new();
        self.records.iter_sync(|repo, _| {
            live.insert(*repo);
            true
        });
        live
    }

    fn apply_delta(&self, interner: &Interner, changes: Vec<RegistryChange>) -> Vec<RepoKey> {
        changes
            .into_iter()
            .fold(Vec::new(), |displaced, change| match change {
                RegistryChange::Register(registration) => {
                    self.apply_register(interner, registration, displaced)
                }
                RegistryChange::Rename(rename) => self.apply_rename(interner, rename, displaced),
                RegistryChange::Deregister(target) => {
                    self.apply_deregister(interner, target, displaced)
                }
            })
    }

    fn apply_register(
        &self,
        interner: &Interner,
        registration: Registration,
        mut displaced: Vec<RepoKey>,
    ) -> Vec<RepoKey> {
        let repo = interner.intern_repo(&registration.repo);
        let owner = interner.intern_owner(&registration.owner);
        let rkey = interner.intern_rkey(&registration.rkey);
        let name = interner.intern_name(&registration.name);
        if self.records.contains_sync(&repo) {
            self.drop_record(repo);
            displaced.push(repo);
        }
        displaced.extend(self.steal_alias(owner, rkey, repo));
        self.upsert_record(repo, owner, rkey, name, registration.created_at);
        self.upsert_alias(owner, rkey, repo);
        self.bind_name(owner, name, registration.created_at, repo);
        displaced
    }

    fn apply_rename(
        &self,
        interner: &Interner,
        rename: Rename,
        mut displaced: Vec<RepoKey>,
    ) -> Vec<RepoKey> {
        let repo = interner.intern_repo(&rename.repo);
        let owner = interner.intern_owner(&rename.owner);
        let rkey = interner.intern_rkey(&rename.rkey);
        let name = interner.intern_name(&rename.name);
        let held = self
            .records
            .read_sync(&repo, |_, slot| {
                (slot.owner == owner).then_some((slot.created_at, slot.name))
            })
            .flatten();
        let Some((created_at, previous)) = held else {
            return displaced;
        };
        displaced.extend(self.steal_alias(owner, rkey, repo));
        self.upsert_record(repo, owner, rkey, name, created_at);
        self.upsert_alias(owner, rkey, repo);
        self.bind_name(owner, name, created_at, repo);
        if previous != name {
            self.unbind_name(owner, previous, created_at, repo);
        }
        displaced
    }

    fn apply_deregister(
        &self,
        interner: &Interner,
        target: RepoRef,
        mut displaced: Vec<RepoKey>,
    ) -> Vec<RepoKey> {
        let Some(owner) = interner.owner(&target.owner) else {
            return displaced;
        };
        let Some(rkey) = interner.rkey(&target.rkey) else {
            return displaced;
        };
        let Some(repo) = self.aliases.read_sync(&(owner, rkey), |_, repo| *repo) else {
            return displaced;
        };
        self.drop_record(repo);
        displaced.push(repo);
        displaced
    }

    fn steal_alias(&self, owner: OwnerKey, rkey: RkeyKey, target: RepoKey) -> Option<RepoKey> {
        let holder = self.aliases.read_sync(&(owner, rkey), |_, repo| *repo)?;
        if holder == target {
            return None;
        }
        let canonical = self
            .records
            .read_sync(&holder, |_, slot| slot.rkey == rkey)
            .unwrap_or(false);
        if canonical {
            self.drop_record(holder);
            Some(holder)
        } else {
            let _ = self.aliases.remove_sync(&(owner, rkey));
            None
        }
    }

    fn drop_record(&self, repo: RepoKey) {
        if let Some((_, slot)) = self.records.remove_sync(&repo) {
            self.unbind_name(slot.owner, slot.name, slot.created_at, repo);
        }
        self.aliases.retain_sync(|_, holder| *holder != repo);
    }

    fn bind_name(&self, owner: OwnerKey, name: NameKey, created_at: UnixSeconds, repo: RepoKey) {
        match self.names.entry_sync((owner, name)) {
            scc::hash_map::Entry::Occupied(mut occupied) => {
                occupied.get_mut().insert((created_at, repo));
            }
            scc::hash_map::Entry::Vacant(vacant) => {
                vacant.insert_entry(BTreeSet::from([(created_at, repo)]));
            }
        }
    }

    fn unbind_name(&self, owner: OwnerKey, name: NameKey, created_at: UnixSeconds, repo: RepoKey) {
        let _ = self.names.remove_if_sync(&(owner, name), |registered| {
            registered.remove(&(created_at, repo));
            registered.is_empty()
        });
    }

    fn upsert_record(
        &self,
        repo: RepoKey,
        owner: OwnerKey,
        rkey: RkeyKey,
        name: NameKey,
        created_at: UnixSeconds,
    ) {
        if self
            .records
            .update_sync(&repo, |_, slot| {
                slot.owner = owner;
                slot.rkey = rkey;
                slot.name = name;
                slot.created_at = created_at;
            })
            .is_none()
        {
            let _ = self.records.insert_sync(
                repo,
                RecordSlot {
                    owner,
                    rkey,
                    name,
                    created_at,
                },
            );
        }
    }

    fn upsert_alias(&self, owner: OwnerKey, rkey: RkeyKey, repo: RepoKey) {
        let key = (owner, rkey);
        if self
            .aliases
            .update_sync(&key, |_, slot| *slot = repo)
            .is_none()
        {
            let _ = self.aliases.insert_sync(key, repo);
        }
    }
}

enum Reading {
    Published(Vec<Arc<OfferedKey>>),
    Unheld,
    Unread,
}

impl Reading {
    fn keys(&self) -> &[Arc<OfferedKey>] {
        match self {
            Reading::Published(keys) => keys,
            Reading::Unheld | Reading::Unread => &[],
        }
    }

    fn is_published(&self) -> bool {
        matches!(self, Reading::Published(_))
    }
}

struct Held {
    reading: Reading,
    lease: KeyLease,
}

const PER_KEY_OVERHEAD: usize = 192;

const PER_ACCOUNT_OVERHEAD: usize = 128;

impl Held {
    fn bytes(&self) -> usize {
        PER_ACCOUNT_OVERHEAD
            + self
                .reading
                .keys()
                .iter()
                .map(|key| key.as_bytes().len() + PER_KEY_OVERHEAD)
                .sum::<usize>()
    }

    fn answers(&self, now: UnixSeconds) -> bool {
        self.reading.is_published() && self.lease.is_live(now)
    }

    fn settled(&self, now: UnixSeconds) -> bool {
        !matches!(self.reading, Reading::Unread) && self.lease.is_live(now)
    }

    fn renewal_due(&self, now: UnixSeconds) -> bool {
        match self.reading {
            Reading::Published(_) => self.lease.renewal_due(now),
            Reading::Unheld | Reading::Unread => !self.lease.is_live(now),
        }
    }

    fn publishes(&self, key: &OfferedKey, now: UnixSeconds) -> bool {
        self.answers(now)
            && self
                .reading
                .keys()
                .iter()
                .any(|published| published.as_ref() == key)
    }

    fn is_unheld(&self) -> bool {
        matches!(self.reading, Reading::Unheld)
    }
}

#[derive(Default)]
struct Publishers(Vec<AccountKey>);

impl Publishers {
    fn add(&mut self, account: AccountKey) {
        if let Err(at) = self.0.binary_search(&account) {
            self.0.insert(at, account);
        }
    }

    fn remove(&mut self, account: AccountKey) -> bool {
        if let Ok(at) = self.0.binary_search(&account) {
            self.0.remove(at);
        }
        self.0.is_empty()
    }

    fn to_vec(&self) -> Vec<AccountKey> {
        self.0.clone()
    }
}

const NEVER: u64 = u64::MAX;

pub(crate) struct KeyProjection {
    owners: scc::HashMap<Arc<OfferedKey>, Publishers>,
    held: scc::HashMap<AccountKey, Held>,
    budget: KeyBudget,
    tracked_bytes: AtomicUsize,
    unheld: AtomicUsize,
    suspect: AtomicBool,
    swept_at: AtomicI64,
    ready_at: AtomicU64,
}

impl KeyProjection {
    pub(crate) fn new(budget: KeyBudget) -> Self {
        Self {
            owners: scc::HashMap::new(),
            held: scc::HashMap::new(),
            budget,
            tracked_bytes: AtomicUsize::new(0),
            unheld: AtomicUsize::new(0),
            suspect: AtomicBool::new(false),
            swept_at: AtomicI64::new(i64::MIN),
            ready_at: AtomicU64::new(NEVER),
        }
    }

    pub(crate) fn coverage(&self, generation: IndexGeneration) -> Coverage {
        match self.ready_at.load(Ordering::Acquire) == generation.get() {
            true => Coverage::Ready,
            false => Coverage::Warming,
        }
    }

    pub(crate) fn suspect_now(&self) {
        self.suspect.store(true, Ordering::Release);
    }

    pub(crate) fn take_suspicion(&self, now: UnixSeconds, floor: SweepFloor) -> bool {
        let held_until = |swept: i64| swept.saturating_add(crate::whole_secs(floor.get()));
        match self.suspect.load(Ordering::Acquire) {
            false => false,
            true => {
                self.swept_at
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |swept| {
                        (now.get() >= held_until(swept)).then_some(now.get())
                    })
                    .is_ok()
                    && self.suspect.swap(false, Ordering::AcqRel)
            }
        }
    }

    pub(crate) fn mark_ready(&self, generation: IndexGeneration) {
        self.ready_at.store(generation.get(), Ordering::Release);
    }

    pub(crate) fn mark_warming(&self) {
        self.ready_at.store(NEVER, Ordering::Release);
    }

    pub(crate) fn owner(
        &self,
        interner: &Interner,
        key: &OfferedKey,
        now: UnixSeconds,
    ) -> Resolved<Option<AccountDid>> {
        let publishers = self
            .owners
            .read_sync(key, |_, publishers| publishers.to_vec())
            .unwrap_or_default();
        Resolved::Ready(
            publishers
                .into_iter()
                .find(|account| {
                    self.held_by(*account, |held| held.publishes(key, now)) == Some(true)
                })
                .map(|account| interner.resolve_account(account)),
        )
    }

    pub(crate) fn any_unheld(&self) -> bool {
        self.unheld.load(Ordering::Acquire) > 0
    }

    fn track_unheld(&self, lost: bool, gained: bool) {
        match (lost, gained) {
            (false, true) => {
                self.unheld.fetch_add(1, Ordering::AcqRel);
            }
            (true, false) => {
                self.unheld.fetch_sub(1, Ordering::AcqRel);
            }
            (true, true) | (false, false) => {}
        }
    }

    pub(crate) fn record(
        &self,
        interner: &Interner,
        did: &AccountDid,
        keys: Vec<OfferedKey>,
        lease: KeyLease,
    ) -> KeyRecord {
        let account = interner.intern_account(did);
        let incoming = Held {
            reading: Reading::Published(keys.into_iter().map(Arc::new).collect()),
            lease,
        };
        let entry = self.held.entry_sync(account);
        let held = match &entry {
            scc::hash_map::Entry::Occupied(slot) => slot.get().bytes(),
            scc::hash_map::Entry::Vacant(_) => 0,
        };
        if self.reserve_bytes(incoming.bytes() as isize - held as isize) {
            self.settle(entry, account, incoming);
            return KeyRecord::Stored;
        }
        let unheld = Held {
            reading: Reading::Unheld,
            lease,
        };
        if !self.reserve_bytes(unheld.bytes() as isize - held as isize) {
            return KeyRecord::Saturated;
        }
        self.settle(entry, account, unheld);
        KeyRecord::Unheld
    }

    fn settle(
        &self,
        entry: scc::hash_map::Entry<'_, AccountKey, Held>,
        account: AccountKey,
        incoming: Held,
    ) {
        let published: HashSet<Arc<OfferedKey>> = incoming.reading.keys().iter().cloned().collect();
        let gained = incoming.is_unheld();
        match entry {
            scc::hash_map::Entry::Occupied(mut slot) => {
                let replaced = slot.insert(incoming);
                self.track_unheld(replaced.is_unheld(), gained);
                self.rewire(account, &published, replaced.reading.keys());
            }
            scc::hash_map::Entry::Vacant(slot) => {
                let _locked = slot.insert_entry(incoming);
                self.track_unheld(false, gained);
                self.rewire(account, &published, &[]);
            }
        }
    }

    fn rewire(
        &self,
        account: AccountKey,
        published: &HashSet<Arc<OfferedKey>>,
        replaced: &[Arc<OfferedKey>],
    ) {
        published
            .iter()
            .for_each(|key| self.claim(Arc::clone(key), account));
        replaced
            .iter()
            .filter(|key| !published.contains(*key))
            .for_each(|key| self.disown(key, account));
    }

    pub(crate) fn reprieve(
        &self,
        interner: &Interner,
        did: &AccountDid,
        now: UnixSeconds,
        grace: KeyReprieve,
        exhausted: KeyLease,
    ) -> KeyReprieved {
        let account = interner.intern_account(did);
        let mut slot = match self.held.entry_sync(account) {
            scc::hash_map::Entry::Vacant(slot) => {
                let pending = Held {
                    reading: Reading::Unread,
                    lease: grace.first_failure(now),
                };
                if self.reserve_bytes(pending.bytes() as isize) {
                    let _locked = slot.insert_entry(pending);
                }
                return KeyReprieved::Pending;
            }
            scc::hash_map::Entry::Occupied(slot) => slot,
        };
        match grace.extend(slot.get().lease, now) {
            Some(lease) => {
                let carried = slot.get().reading.is_published();
                slot.get_mut().lease = lease;
                match carried {
                    true => KeyReprieved::Extended,
                    false => KeyReprieved::Pending,
                }
            }
            None => {
                let given_up = Held {
                    reading: Reading::Published(Vec::new()),
                    lease: exhausted,
                };
                let _ = self.reserve_bytes(given_up.bytes() as isize - slot.get().bytes() as isize);
                self.track_unheld(slot.get().is_unheld(), false);
                self.disown_all(&slot, account);
                *slot.get_mut() = given_up;
                KeyReprieved::Exhausted
            }
        }
    }

    pub(crate) fn on_file(&self, interner: &Interner, did: &AccountDid) -> bool {
        interner
            .account(did)
            .is_some_and(|account| self.held.contains_sync(&account))
    }

    fn held_by(&self, account: AccountKey, ready: impl Fn(&Held) -> bool) -> Option<bool> {
        self.held.read_sync(&account, |_, held| ready(held))
    }

    fn held_for(
        &self,
        interner: &Interner,
        did: &AccountDid,
        ready: impl Fn(&Held) -> bool,
    ) -> Option<bool> {
        interner
            .account(did)
            .and_then(|account| self.held_by(account, ready))
    }

    pub(crate) fn is_fresh(&self, interner: &Interner, did: &AccountDid, now: UnixSeconds) -> bool {
        self.held_for(interner, did, |held| held.answers(now))
            .unwrap_or(false)
    }

    pub(crate) fn renewal_due(
        &self,
        interner: &Interner,
        did: &AccountDid,
        now: UnixSeconds,
    ) -> bool {
        self.held_for(interner, did, |held| held.renewal_due(now))
            .unwrap_or(true)
    }

    pub(crate) fn all_live(
        &self,
        interner: &Interner,
        subjects: &[AccountDid],
        now: UnixSeconds,
    ) -> bool {
        subjects.iter().all(|did| {
            self.held_for(interner, did, |held| held.settled(now))
                .unwrap_or(false)
        })
    }

    pub(crate) fn publisher_among(
        &self,
        interner: &Interner,
        candidates: &[AccountDid],
        key: &OfferedKey,
        now: UnixSeconds,
    ) -> Option<AccountDid> {
        candidates
            .iter()
            .find(|did| {
                self.held_for(interner, did, |held| held.publishes(key, now))
                    .unwrap_or(false)
            })
            .cloned()
    }

    pub(crate) fn retain(&self, interner: &Interner, kept: &[AccountDid]) {
        let keep: BTreeSet<AccountKey> = kept
            .iter()
            .filter_map(|did| interner.account(did))
            .collect();
        let mut released = Vec::new();
        self.held.iter_sync(|account, _| {
            if !keep.contains(account) {
                released.push(*account);
            }
            true
        });
        released
            .into_iter()
            .for_each(|account| self.release(account));
    }

    fn release(&self, account: AccountKey) {
        if let scc::hash_map::Entry::Occupied(slot) = self.held.entry_sync(account) {
            self.evict(&slot, account);
            let _ = slot.remove();
        }
    }

    fn evict(
        &self,
        slot: &scc::hash_map::OccupiedEntry<'_, AccountKey, Held>,
        account: AccountKey,
    ) {
        self.track_unheld(slot.get().is_unheld(), false);
        self.disown_all(slot, account);
        let _ = self.reserve_bytes(-(slot.get().bytes() as isize));
    }

    fn disown_all(
        &self,
        slot: &scc::hash_map::OccupiedEntry<'_, AccountKey, Held>,
        account: AccountKey,
    ) {
        slot.get()
            .reading
            .keys()
            .iter()
            .for_each(|key| self.disown(key, account));
    }

    fn reserve_bytes(&self, growth: isize) -> bool {
        self.tracked_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |tracked| {
                let next = tracked.saturating_add_signed(growth);
                (growth <= 0 || next <= self.budget.get()).then_some(next)
            })
            .is_ok()
    }

    fn claim(&self, key: Arc<OfferedKey>, account: AccountKey) {
        self.owners
            .entry_sync(key)
            .or_default()
            .get_mut()
            .add(account);
    }

    fn disown(&self, key: &OfferedKey, account: AccountKey) {
        let _ = self
            .owners
            .remove_if_sync(key, |publishers| publishers.remove(account));
    }
}
