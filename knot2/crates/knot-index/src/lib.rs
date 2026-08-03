mod coverage;
mod error;
mod intern;
mod projections;

pub use coverage::{Coverage, Resolved};
pub use error::IndexError;
pub use knot_types::OfferedKey;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use knot_cob::{ChangePayload, CobStore};
use knot_cobs::{
    BlocklistChange, BlocklistCob, CollaboratorsChange, CollaboratorsCob, Grant, MembersChange,
    MembersCob, RegistryChange, RepoRegistryCob,
};
use knot_git::{Layout, Repo};
use knot_types::{AccountDid, ClonePath, OwnerDid, RepoDid, RepoRkey, UnixSeconds};
use tokio::sync::watch;

use intern::{Interner, RepoKey};
use projections::{CollaboratorsProjection, GrantSetProjection, KeyProjection, RegistryProjection};

knot_types::scalar_newtype! {
    pub struct IndexGeneration(u64);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexCoverage {
    pub members: Coverage,
    pub blocklist: Coverage,
    pub collaborators: Coverage,
    pub registry: Coverage,
    pub keys: Coverage,
}

macro_rules! account_list {
    ($($name:ident),+ $(,)?) => {$(
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $name(Vec<AccountDid>);

        impl $name {
            pub fn new(accounts: Vec<AccountDid>) -> Self {
                Self(accounts)
            }

            pub fn len(&self) -> usize {
                self.0.len()
            }

            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }

            pub fn as_slice(&self) -> &[AccountDid] {
                &self.0
            }

            pub fn into_vec(self) -> Vec<AccountDid> {
                self.0
            }
        }
    )+};
}

account_list!(
    Pushers,
    KeptAccounts,
    StalePushers,
    SuspectPushers,
    UnreadMembers,
    StaleMembers,
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostedCoverage {
    Whole,
    Partial { unread: usize },
}

impl HostedCoverage {
    const fn over(unread: usize) -> Self {
        match unread {
            0 => Self::Whole,
            unread => Self::Partial { unread },
        }
    }
}

struct Granted {
    subjects: Vec<AccountDid>,
    hosted: HostedCoverage,
}

enum Folded {
    Grants(Vec<AccountDid>),
    Pending,
    Unreadable,
}

impl Folded {
    fn into_grants(self) -> Option<Vec<AccountDid>> {
        match self {
            Folded::Grants(subjects) => Some(subjects),
            Folded::Pending | Folded::Unreadable => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepFloor(Duration);

impl SweepFloor {
    pub const DEFAULT: Self = Self::from_secs(60);

    pub const fn from_secs(secs: u64) -> Self {
        Self(Duration::from_secs(secs))
    }

    pub const fn get(self) -> Duration {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyTtl(Duration);

impl KeyTtl {
    pub const DEFAULT: Self = Self::from_secs(3_600);

    pub const fn from_secs(secs: u64) -> Self {
        Self(Duration::from_secs(secs))
    }

    pub const fn get(self) -> Duration {
        self.0
    }

    pub const fn longest(self, other: Self) -> Self {
        match self.0.as_secs() >= other.0.as_secs() {
            true => self,
            false => other,
        }
    }

    pub const fn lease_from(self, now: UnixSeconds) -> KeyLease {
        KeyLease {
            read_at: now,
            expires_at: after(now, self.0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyLease {
    read_at: UnixSeconds,
    expires_at: UnixSeconds,
}

impl KeyLease {
    pub(crate) const fn is_live(self, now: UnixSeconds) -> bool {
        self.expires_at.get() > now.get()
    }

    pub(crate) const fn renewal_due(self, now: UnixSeconds) -> bool {
        let held = self.expires_at.get().saturating_sub(self.read_at.get());
        now.get() >= self.read_at.saturating_add_secs(held / 2).get()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyReprieve {
    retry: Duration,
    budget: Duration,
}

impl KeyReprieve {
    pub const DEFAULT: Self = Self::from_secs(300, 21_600);

    pub const fn from_secs(retry: u64, budget: u64) -> Self {
        Self {
            retry: Duration::from_secs(retry),
            budget: Duration::from_secs(budget),
        }
    }

    pub const fn budgeted_for(self, ttl: KeyTtl) -> Self {
        match self.budget.as_secs() >= ttl.0.as_secs() {
            true => self,
            false => Self {
                retry: self.retry,
                budget: ttl.0,
            },
        }
    }

    pub(crate) const fn first_failure(self, now: UnixSeconds) -> KeyLease {
        KeyLease {
            read_at: now,
            expires_at: after(now, self.retry),
        }
    }

    pub(crate) const fn extend(self, lease: KeyLease, now: UnixSeconds) -> Option<KeyLease> {
        let horizon = after(lease.read_at, self.budget);
        match horizon.get() > now.get() {
            false => None,
            true => {
                let retry = after(now, self.retry);
                let granted = match retry.get() < horizon.get() {
                    true => retry,
                    false => horizon,
                };
                Some(KeyLease {
                    read_at: lease.read_at,
                    expires_at: match lease.expires_at.get() > granted.get() {
                        true => lease.expires_at,
                        false => granted,
                    },
                })
            }
        }
    }
}

pub(crate) const fn whole_secs(span: Duration) -> i64 {
    let secs = span.as_secs();
    match secs > i64::MAX as u64 {
        true => i64::MAX,
        false => secs as i64,
    }
}

const fn after(now: UnixSeconds, span: Duration) -> UnixSeconds {
    now.saturating_add_secs(whole_secs(span))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRecord {
    Stored,
    Unheld,
    Saturated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyReprieved {
    Extended,
    Pending,
    Exhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyBudget(usize);

impl KeyBudget {
    pub const DEFAULT: Self = Self::from_mib(64);

    pub const fn from_mib(mib: usize) -> Self {
        Self(mib * 1024 * 1024)
    }

    pub const fn from_bytes(bytes: usize) -> Self {
        Self(bytes)
    }

    pub const fn get(self) -> usize {
        self.0
    }
}

pub struct MemberWork {
    pub unread: UnreadMembers,
    pub due: StaleMembers,
    pub kept: KeptAccounts,
}

pub struct KeyWork {
    pub generation: IndexGeneration,
    pub tracked: usize,
    pub hosted: HostedCoverage,
    pub pushers: Pushers,
    pub due: StalePushers,
    pub suspected: SuspectPushers,
    pub complete: bool,
    pub members: Resolved<MemberWork>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recheck {
    Renewals,
    Everything,
}

fn sorted(mut subjects: Vec<AccountDid>) -> Vec<AccountDid> {
    subjects.sort();
    subjects.dedup();
    subjects
}

pub struct Index {
    meta_path: PathBuf,
    layout: Layout,
    interner: Interner,
    members: GrantSetProjection<MembersCob>,
    blocklist: GrantSetProjection<BlocklistCob>,
    collaborators: CollaboratorsProjection,
    registry: RegistryProjection,
    keys: KeyProjection,
    unreadable: scc::HashSet<RepoKey>,
    generation: AtomicU64,
    generations: watch::Sender<IndexGeneration>,
}

impl Index {
    pub fn new(meta_path: impl Into<PathBuf>, layout: Layout) -> Self {
        Self::with_key_budget(meta_path, layout, KeyBudget::DEFAULT)
    }

    pub fn with_key_budget(
        meta_path: impl Into<PathBuf>,
        layout: Layout,
        budget: KeyBudget,
    ) -> Self {
        Self {
            meta_path: meta_path.into(),
            layout,
            interner: Interner::new(),
            members: GrantSetProjection::new(),
            blocklist: GrantSetProjection::new(),
            collaborators: CollaboratorsProjection::new(),
            registry: RegistryProjection::new(),
            keys: KeyProjection::new(budget),
            unreadable: scc::HashSet::new(),
            generation: AtomicU64::new(0),
            generations: watch::Sender::new(IndexGeneration::new(0)),
        }
    }

    pub fn generation(&self) -> IndexGeneration {
        IndexGeneration(self.generation.load(Ordering::Acquire))
    }

    pub fn generations(&self) -> watch::Receiver<IndexGeneration> {
        self.generations.subscribe()
    }

    fn bump_generation(&self) {
        self.generation.fetch_add(1, Ordering::Release);
        self.generations.send_replace(self.generation());
    }

    pub fn rebuild(&self) -> Result<(), IndexError> {
        self.refresh_members()?;
        self.refresh_blocklist()?;
        self.refresh_registry()?;
        Ok(())
    }

    pub fn warm_collaborators(&self) -> usize {
        self.hosted_repos()
            .iter()
            .filter(|repo| match self.ensure_collaborators(repo) {
                Ok(()) => false,
                Err(error) => {
                    tracing::warn!(
                        repo = repo.as_str(),
                        %error,
                        "collaborators unread, the knot can't open the repo"
                    );
                    true
                }
            })
            .count()
    }

    fn unreadable_repo(&self, repo: &RepoDid) -> bool {
        self.interner
            .repo(repo)
            .is_some_and(|repo| self.unreadable.contains_sync(&repo))
    }

    pub fn refresh_members(&self) -> Result<(), IndexError> {
        let meta = Repo::open(&self.meta_path)?;
        let store = CobStore::new(&meta);
        match store.list::<MembersCob>()?.as_slice() {
            [] => self.members.reset(),
            [object] => self.members.refresh(&self.interner, &store, *object)?,
            many => {
                return Err(IndexError::Ambiguous {
                    type_name: MembersChange::type_name(),
                    count: many.len(),
                });
            }
        }
        self.bump_generation();
        Ok(())
    }

    pub fn refresh_blocklist(&self) -> Result<(), IndexError> {
        let meta = Repo::open(&self.meta_path)?;
        let store = CobStore::new(&meta);
        match store.list::<BlocklistCob>()?.as_slice() {
            [] => self.blocklist.reset(),
            [object] => self.blocklist.refresh(&self.interner, &store, *object)?,
            many => {
                return Err(IndexError::Ambiguous {
                    type_name: BlocklistChange::type_name(),
                    count: many.len(),
                });
            }
        }
        self.bump_generation();
        Ok(())
    }

    pub fn refresh_registry(&self) -> Result<(), IndexError> {
        let meta = Repo::open(&self.meta_path)?;
        let store = CobStore::new(&meta);
        let evacuated = match store.list::<RepoRegistryCob>()?.as_slice() {
            [] => self.registry.reset(&self.interner),
            [object] => self.registry.refresh(&self.interner, &store, *object)?,
            many => {
                return Err(IndexError::Ambiguous {
                    type_name: RegistryChange::type_name(),
                    count: many.len(),
                });
            }
        };
        evacuated.iter().for_each(|repo| {
            if let Some(key) = self.interner.repo(repo) {
                self.collaborators.drop_repo(key);
                self.unreadable.remove_sync(&key);
            }
        });
        self.bump_generation();
        Ok(())
    }

    pub fn ensure_collaborators(&self, repo: &RepoDid) -> Result<(), IndexError> {
        if self.collaborators.is_folded(&self.interner, repo) {
            return Ok(());
        }
        self.refresh_collaborators(repo)
    }

    pub fn refresh_collaborators(&self, repo: &RepoDid) -> Result<(), IndexError> {
        let repo_key = self.interner.intern_repo(repo);
        self.fold_collaborators(repo, repo_key)
            .inspect(|()| {
                self.unreadable.remove_sync(&repo_key);
            })
            .inspect_err(|_| {
                let _ = self.unreadable.insert_sync(repo_key);
            })
    }

    fn fold_collaborators(&self, repo: &RepoDid, repo_key: RepoKey) -> Result<(), IndexError> {
        let git = self.layout.open(repo)?;
        let store = CobStore::new(&git);
        match store.list::<CollaboratorsCob>()?.as_slice() {
            [] => self.collaborators.mark_repo_empty(repo_key),
            [object] => {
                self.collaborators
                    .refresh_repo(&self.interner, &store, repo_key, *object)?
            }
            many => {
                return Err(IndexError::Ambiguous {
                    type_name: CollaboratorsChange::type_name(),
                    count: many.len(),
                });
            }
        }
        self.bump_generation();
        Ok(())
    }

    pub fn is_member(&self, did: &AccountDid) -> Resolved<bool> {
        self.members.contains(&self.interner, did)
    }

    pub fn member_entries(&self) -> Resolved<Vec<Grant>> {
        self.members.entries(&self.interner)
    }

    pub fn is_blocked(&self, did: &AccountDid) -> Resolved<bool> {
        self.blocklist.contains(&self.interner, did)
    }

    pub fn blocked_entries(&self) -> Resolved<Vec<Grant>> {
        self.blocklist.entries(&self.interner)
    }

    pub fn is_collaborator(&self, repo: &RepoDid, did: &AccountDid) -> Resolved<bool> {
        self.collaborators.contains(&self.interner, repo, did)
    }

    pub fn collaborator_entries(&self, repo: &RepoDid) -> Resolved<Vec<Grant>> {
        self.collaborators.entries(&self.interner, repo)
    }

    pub fn collaborators_of(&self, repo: &RepoDid) -> Resolved<Vec<AccountDid>> {
        self.collaborator_entries(repo)
            .map(|entries| entries.into_iter().map(|grant| grant.subject).collect())
    }

    pub fn resolve_repo(&self, owner: &OwnerDid, rkey: &RepoRkey) -> Resolved<Option<RepoDid>> {
        self.registry.resolve(&self.interner, owner, rkey)
    }

    pub fn resolve_clone_path(
        &self,
        owner: &OwnerDid,
        path: &ClonePath,
    ) -> Resolved<Option<RepoDid>> {
        self.registry
            .resolve_clone_path(&self.interner, owner, path)
    }

    pub fn owner_of(&self, repo: &RepoDid) -> Resolved<Option<OwnerDid>> {
        self.registry.owner_of(&self.interner, repo)
    }

    pub fn rkey_of(&self, repo: &RepoDid) -> Resolved<Option<RepoRkey>> {
        self.registry.rkey_of(&self.interner, repo)
    }

    pub fn hosted_repos(&self) -> Vec<RepoDid> {
        self.registry.hosted_repos(&self.interner)
    }

    pub fn owner_of_key(&self, key: &OfferedKey, now: UnixSeconds) -> Resolved<Option<AccountDid>> {
        self.keys.owner(&self.interner, key, now)
    }

    pub fn keys(&self) -> KeySet<'_> {
        KeySet(self)
    }

    fn push_grants(&self) -> Resolved<Granted> {
        match self.registry.coverage() {
            Coverage::Warming => Resolved::Warming,
            Coverage::Ready => {
                let folded: Vec<Folded> = self
                    .hosted_repos()
                    .iter()
                    .map(
                        |repo| match (self.owner_of(repo), self.collaborators_of(repo)) {
                            (Resolved::Ready(owner), Resolved::Ready(collaborators)) => {
                                Folded::Grants(
                                    owner
                                        .map(AccountDid::from)
                                        .into_iter()
                                        .chain(collaborators)
                                        .collect(),
                                )
                            }
                            _ if self.unreadable_repo(repo) => Folded::Unreadable,
                            _ => Folded::Pending,
                        },
                    )
                    .collect();
                let hosted = HostedCoverage::over(
                    folded
                        .iter()
                        .filter(|repo| matches!(repo, Folded::Pending))
                        .count(),
                );
                Resolved::Ready(Granted {
                    subjects: sorted(
                        folded
                            .into_iter()
                            .filter_map(Folded::into_grants)
                            .flatten()
                            .collect(),
                    ),
                    hosted,
                })
            }
        }
    }

    fn member_grants(&self) -> Resolved<Vec<AccountDid>> {
        self.member_entries()
            .map(|entries| sorted(entries.into_iter().map(|grant| grant.subject).collect()))
    }

    fn due_among(
        &self,
        subjects: &[AccountDid],
        now: UnixSeconds,
        against: Recheck,
    ) -> Vec<AccountDid> {
        subjects
            .iter()
            .filter(|did| match against {
                Recheck::Everything => true,
                Recheck::Renewals => self.keys.renewal_due(&self.interner, did, now),
            })
            .cloned()
            .collect()
    }

    pub fn coverage(&self) -> IndexCoverage {
        IndexCoverage {
            members: self.members.coverage(),
            blocklist: self.blocklist.coverage(),
            collaborators: self.collaborators.coverage(),
            registry: self.registry.coverage(),
            keys: self.keys.coverage(self.generation()),
        }
    }
}

pub struct KeySet<'a>(&'a Index);

impl KeySet<'_> {
    pub fn coverage(&self) -> Coverage {
        self.0.keys.coverage(self.0.generation())
    }

    pub fn mark_ready(&self, generation: IndexGeneration) {
        self.0.keys.mark_ready(generation);
    }

    pub fn mark_warming(&self) {
        self.0.keys.mark_warming();
    }

    pub fn record(&self, did: &AccountDid, keys: Vec<OfferedKey>, lease: KeyLease) -> KeyRecord {
        self.0.keys.record(&self.0.interner, did, keys, lease)
    }

    pub fn reprieve(
        &self,
        did: &AccountDid,
        now: UnixSeconds,
        grace: KeyReprieve,
        exhausted: KeyLease,
    ) -> KeyReprieved {
        self.0
            .keys
            .reprieve(&self.0.interner, did, now, grace, exhausted)
    }

    pub fn retain(&self, kept: &KeptAccounts) {
        self.0.keys.retain(&self.0.interner, kept.as_slice());
    }

    pub fn is_fresh(&self, did: &AccountDid, now: UnixSeconds) -> bool {
        self.0.keys.is_fresh(&self.0.interner, did, now)
    }

    pub fn publisher_among(
        &self,
        candidates: &[AccountDid],
        key: &OfferedKey,
        now: UnixSeconds,
    ) -> Option<AccountDid> {
        self.0
            .keys
            .publisher_among(&self.0.interner, candidates, key, now)
    }

    pub fn note_miss(&self) {
        self.0.keys.suspect_now();
    }

    pub fn any_unheld(&self) -> bool {
        self.0.keys.any_unheld()
    }

    pub fn work(&self, now: UnixSeconds, floor: SweepFloor) -> Resolved<KeyWork> {
        let index = self.0;
        let generation = index.generation();
        let Resolved::Ready(granted) = index.push_grants() else {
            return Resolved::Warming;
        };
        let pushers = granted.subjects;
        let against = match index.keys.take_suspicion(now, floor) {
            true => Recheck::Everything,
            false => Recheck::Renewals,
        };
        let members = index.member_grants().map(|members| {
            let outside: Vec<AccountDid> = members
                .into_iter()
                .filter(|did| pushers.binary_search(did).is_err())
                .collect();
            let (unread, due) = index
                .due_among(&outside, now, against)
                .into_iter()
                .partition(|did| !index.keys.on_file(&index.interner, did));
            MemberWork {
                unread: UnreadMembers(unread),
                due: StaleMembers(due),
                kept: KeptAccounts(pushers.iter().cloned().chain(outside).collect()),
            }
        });
        let tracked = match &members {
            Resolved::Ready(work) => work.kept.len(),
            Resolved::Warming => pushers.len(),
        };
        let due = index.due_among(&pushers, now, Recheck::Renewals);
        let suspected = match against {
            Recheck::Renewals => Vec::new(),
            Recheck::Everything => pushers
                .iter()
                .filter(|did| !index.keys.renewal_due(&index.interner, did, now))
                .cloned()
                .collect(),
        };
        let complete = granted.hosted == HostedCoverage::Whole
            && index.keys.all_live(&index.interner, &pushers, now);
        Resolved::Ready(KeyWork {
            generation,
            tracked,
            hosted: granted.hosted,
            pushers: Pushers(pushers),
            due: StalePushers(due),
            suspected: SuspectPushers(suspected),
            complete,
            members,
        })
    }

    pub fn all_live(&self, pushers: &Pushers, now: UnixSeconds) -> bool {
        self.0
            .keys
            .all_live(&self.0.interner, pushers.as_slice(), now)
    }
}
