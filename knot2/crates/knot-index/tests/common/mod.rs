#![allow(dead_code)]

use std::path::PathBuf;

use knot_cob::{CobHome, CobId, CobStore};
use knot_cobs::{CollaboratorsChange, Grant, MembersChange, Registration, RegistryChange, Removal};
use knot_git::{Layout, Repo};
use knot_index::Index;
use knot_runtime::{K256Signer, SeededEntropy};
use knot_types::{AccountDid, KnotId, OwnerDid, RepoDid, RepoName, RepoRkey, UnixSeconds};
use tempfile::TempDir;

pub fn acc(suffix: &str) -> AccountDid {
    AccountDid::new(format!("did:plc:{suffix}")).unwrap()
}

pub fn own(suffix: &str) -> OwnerDid {
    OwnerDid::new(format!("did:plc:{suffix}")).unwrap()
}

pub fn repo_did(suffix: &str) -> RepoDid {
    RepoDid::new(format!("did:plc:{suffix}")).unwrap()
}

pub fn rkey(value: &str) -> RepoRkey {
    RepoRkey::new(value).unwrap()
}

pub fn at(seconds: i64) -> UnixSeconds {
    UnixSeconds::new(seconds)
}

pub fn meta_home() -> CobHome {
    CobHome::from(&KnotId::new("did:web:knot.nel.pet").unwrap())
}

pub fn grant(subject: &str, added_by: &str, seconds: i64) -> Grant {
    Grant {
        subject: acc(subject),
        added_by: acc(added_by),
        created_at: at(seconds),
    }
}

pub fn registration(owner_id: &str, key: &str, repo: &RepoDid, seconds: i64) -> Registration {
    Registration {
        owner: own(owner_id),
        rkey: rkey(key),
        name: RepoName::new(key).unwrap(),
        repo: repo.clone(),
        created_at: at(seconds),
    }
}

pub fn named_registration(
    owner_id: &str,
    key: &str,
    display: &str,
    repo: &RepoDid,
    seconds: i64,
) -> Registration {
    Registration {
        owner: own(owner_id),
        rkey: rkey(key),
        name: RepoName::new(display).unwrap(),
        repo: repo.clone(),
        created_at: at(seconds),
    }
}

pub struct World {
    _dir: TempDir,
    pub meta_path: PathBuf,
    pub layout: Layout,
    pub signer: K256Signer,
}

impl World {
    pub fn new() -> Self {
        Self::seeded(1)
    }

    pub fn seeded(seed: u64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta");
        Repo::create(&meta_path).unwrap();
        let layout = Layout::new(dir.path().join("repos"));
        let signer = K256Signer::generate(&SeededEntropy::new(seed));
        Self {
            _dir: dir,
            meta_path,
            layout,
            signer,
        }
    }

    pub fn index(&self) -> Index {
        Index::new(&self.meta_path, self.layout.clone())
    }

    pub fn seed_members(&self) -> CobId {
        let meta = Repo::open(&self.meta_path).unwrap();
        let store = CobStore::new(&meta);
        let created = store
            .create(
                &meta_home(),
                &MembersChange::Add(grant("nel", "nel", 1)),
                &self.signer,
                at(1),
            )
            .unwrap();
        store
            .update(
                &meta_home(),
                created.object,
                &MembersChange::Add(grant("olaren", "nel", 2)),
                &self.signer,
                at(2),
            )
            .unwrap();
        created.object
    }

    pub fn add_member(&self, object: CobId, subject: &str, seconds: i64) {
        let meta = Repo::open(&self.meta_path).unwrap();
        let store = CobStore::new(&meta);
        store
            .update(
                &meta_home(),
                object,
                &MembersChange::Add(grant(subject, "nel", seconds)),
                &self.signer,
                at(seconds),
            )
            .unwrap();
    }

    pub fn seed_registry(&self, repo: &RepoDid) -> CobId {
        let meta = Repo::open(&self.meta_path).unwrap();
        let store = CobStore::new(&meta);
        store
            .create(
                &meta_home(),
                &RegistryChange::Register(registration("nel", "anemone", repo, 1)),
                &self.signer,
                at(1),
            )
            .unwrap()
            .object
    }

    pub fn register_extra(&self, repo: &RepoDid, key: &str, registry: CobId) {
        let meta = Repo::open(&self.meta_path).unwrap();
        let store = CobStore::new(&meta);
        store
            .update(
                &meta_home(),
                registry,
                &RegistryChange::Register(registration("nel", key, repo, 2)),
                &self.signer,
                at(2),
            )
            .unwrap();
    }

    pub fn seed_collaborator(&self, repo: &RepoDid, subject: &str) -> CobId {
        let git = self.layout.create(repo).unwrap();
        let store = CobStore::new(&git);
        store
            .create(
                &CobHome::from(repo),
                &CollaboratorsChange::Add(grant(subject, "nel", 1)),
                &self.signer,
                at(1),
            )
            .unwrap()
            .object
    }

    pub fn remove_collaborator(&self, repo: &RepoDid, object: CobId, subject: &str, seconds: i64) {
        let git = self.layout.open(repo).unwrap();
        let store = CobStore::new(&git);
        store
            .update(
                &CobHome::from(repo),
                object,
                &CollaboratorsChange::Remove(Removal {
                    subject: acc(subject),
                }),
                &self.signer,
                at(seconds),
            )
            .unwrap();
    }
}
