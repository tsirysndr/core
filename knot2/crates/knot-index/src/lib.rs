mod coverage;
mod error;
mod intern;
mod projections;

pub use coverage::{Coverage, Resolved};
pub use error::IndexError;
pub use knot_types::OfferedKey;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use knot_cob::{ChangePayload, CobStore};
use knot_cobs::{
    BlocklistChange, BlocklistCob, CollaboratorsChange, CollaboratorsCob, Grant, MembersChange,
    MembersCob, RegistryChange, RepoRegistryCob,
};
use knot_git::{Layout, Repo};
use knot_types::{AccountDid, OwnerDid, RepoDid, RepoRkey};

use intern::Interner;
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

pub struct Index {
    meta_path: PathBuf,
    layout: Layout,
    interner: Interner,
    members: GrantSetProjection<MembersCob>,
    blocklist: GrantSetProjection<BlocklistCob>,
    collaborators: CollaboratorsProjection,
    registry: RegistryProjection,
    keys: KeyProjection,
    generation: AtomicU64,
}

impl Index {
    pub fn new(meta_path: impl Into<PathBuf>, layout: Layout) -> Self {
        Self {
            meta_path: meta_path.into(),
            layout,
            interner: Interner::new(),
            members: GrantSetProjection::new(),
            blocklist: GrantSetProjection::new(),
            collaborators: CollaboratorsProjection::new(),
            registry: RegistryProjection::new(),
            keys: KeyProjection::new(),
            generation: AtomicU64::new(0),
        }
    }

    pub fn generation(&self) -> IndexGeneration {
        IndexGeneration(self.generation.load(Ordering::Acquire))
    }

    fn bump_generation(&self) {
        self.generation.fetch_add(1, Ordering::Release);
    }

    pub fn rebuild(&self) -> Result<(), IndexError> {
        self.refresh_members()?;
        self.refresh_blocklist()?;
        self.refresh_registry()?;
        Ok(())
    }

    pub fn warm_collaborators(&self) {
        self.hosted_repos().iter().for_each(|repo| {
            let _ = self.ensure_collaborators(repo);
        });
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
        let git = self.layout.open(repo)?;
        let store = CobStore::new(&git);
        let repo_key = self.interner.intern_repo(repo);
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

    pub fn owner_of(&self, repo: &RepoDid) -> Resolved<Option<OwnerDid>> {
        self.registry.owner_of(&self.interner, repo)
    }

    pub fn rkey_of(&self, repo: &RepoDid) -> Resolved<Option<RepoRkey>> {
        self.registry.rkey_of(&self.interner, repo)
    }

    pub fn hosted_repos(&self) -> Vec<RepoDid> {
        self.registry.hosted_repos(&self.interner)
    }

    pub fn owner_of_key(&self, key: &OfferedKey) -> Resolved<Option<AccountDid>> {
        self.keys.owner(&self.interner, key)
    }

    pub fn cache_key(&self, key: OfferedKey, did: &AccountDid) {
        self.keys.cache(&self.interner, key, did);
    }

    pub fn coverage(&self) -> IndexCoverage {
        IndexCoverage {
            members: self.members.coverage(),
            blocklist: self.blocklist.coverage(),
            collaborators: self.collaborators.coverage(),
            registry: self.registry.coverage(),
            keys: self.keys.coverage(),
        }
    }
}
