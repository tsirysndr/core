use std::path::{Path, PathBuf};

use knot_cob::{CobError, CobHome, CobId, CobStore};
use knot_cobs::{
    CollaboratorsChange, Grant, MembersChange, MembersCob, Registration, RegistryChange,
    RepoRegistryCob, add_member, register_repo,
};
use knot_git::{
    EntryKind, Identity, Layout, NewCommit, RefUpdate, Repo, StagedAction, StagedChange,
};
use knot_index::Index;
use knot_runtime::{K256Signer, SeededEntropy};
use knot_types::{
    AccountDid, AuthorName, Email, KnotId, Oid, OwnerDid, RefName, RepoDid, RepoName, RepoRkey,
    UnixSeconds,
};
use tempfile::TempDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitCount(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathCount(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChurnCount(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangeCount(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepoCount(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefCount(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RosterCount(u32);

impl CommitCount {
    pub fn new(value: u32) -> Self {
        Self(value.max(1))
    }
    fn get(self) -> u32 {
        self.0
    }
}

impl PathCount {
    pub fn new(value: u32) -> Self {
        Self(value.max(1))
    }
    fn get(self) -> u32 {
        self.0
    }
}

impl ChurnCount {
    pub fn new(value: u32) -> Self {
        Self(value.max(1))
    }
    fn get(self) -> u32 {
        self.0
    }
}

impl ChangeCount {
    pub fn new(value: u32) -> Self {
        Self(value.max(1))
    }
    fn get(self) -> u32 {
        self.0
    }
}

impl RepoCount {
    pub fn new(value: u64) -> Self {
        Self(value.max(1))
    }
    pub fn get(self) -> u64 {
        self.0
    }
}

impl RefCount {
    pub fn new(value: u32) -> Self {
        Self(value.max(1))
    }
    fn get(self) -> u32 {
        self.0
    }
}

impl RosterCount {
    pub fn new(value: u32) -> Self {
        Self(value.max(1))
    }
    fn get(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy)]
pub struct HistorySpec {
    pub commits: CommitCount,
    pub paths: PathCount,
    pub churn: ChurnCount,
}

const CONTENT_BYTES: usize = 128;
const GENESIS_SECONDS: i64 = 1_700_000_000;

fn splitmix(seed: u64) -> u64 {
    let z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    let z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

knot_types::scalar_newtype! {
    struct PathIndex(u32);
    struct Revision(u32);
}

fn blob_content(path_index: PathIndex, revision: Revision) -> Vec<u8> {
    let seed = splitmix(u64::from(path_index.get()) ^ u64::from(revision.get()).rotate_left(32));
    (0..CONTENT_BYTES)
        .scan(seed, |state, _| {
            *state = splitmix(*state);
            Some((*state & 0xff) as u8)
        })
        .collect()
}

fn path_at(path_index: PathIndex) -> knot_types::RepoPath {
    let path_index = path_index.get();
    knot_types::RepoPath::new(format!(
        "src/m{:04}/f{:06}.dat",
        path_index / 256,
        path_index
    ))
    .expect("generated fixture path is well-formed")
}

fn identity(revision: Revision) -> Identity {
    Identity {
        name: AuthorName::new("nel"),
        email: Email::new("nel@oyster.cafe"),
        time: UnixSeconds::new(GENESIS_SECONDS + i64::from(revision.get())),
        offset_seconds: 0,
    }
}

fn put(path_index: PathIndex, revision: Revision) -> StagedChange {
    StagedChange {
        path: path_at(path_index),
        action: StagedAction::Put {
            content: blob_content(path_index, revision),
            kind: EntryKind::Blob,
        },
    }
}

fn churn_indices(revision: Revision, spec: HistorySpec) -> impl Iterator<Item = PathIndex> {
    let span = u64::from(spec.paths.get());
    let base = u64::from(revision.get()).wrapping_mul(u64::from(spec.churn.get()));
    (0..spec.churn.get())
        .map(move |offset| PathIndex::new(((base + u64::from(offset)) % span) as u32))
}

pub struct BuiltHistory {
    _dir: TempDir,
    repo: Repo,
    tip: Oid,
}

impl BuiltHistory {
    pub fn repo(&self) -> &Repo {
        &self.repo
    }
    pub fn tip(&self) -> Oid {
        self.tip
    }
    pub fn tips(&self) -> Vec<Oid> {
        vec![self.tip]
    }
}

pub fn build_history(spec: HistorySpec) -> BuiltHistory {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repo::create(dir.path().join("repo.git")).expect("create repo");
    let tip = write_history(&repo, spec);
    BuiltHistory {
        _dir: dir,
        repo,
        tip,
    }
}

pub fn write_history(repo: &Repo, spec: HistorySpec) -> Oid {
    let empty_tree = Oid::from(repo.git().empty_tree().id().detach());

    let genesis_changes: Vec<StagedChange> = (0..spec.paths.get())
        .map(|index| put(PathIndex::new(index), Revision::new(0)))
        .collect();
    let genesis_tree = repo
        .write_staged_tree(empty_tree, &genesis_changes)
        .expect("genesis tree");
    let genesis_commit = repo
        .write_commit(&NewCommit {
            tree: genesis_tree,
            parents: Vec::new(),
            author: identity(Revision::new(0)),
            committer: identity(Revision::new(0)),
            message: "genesis".to_string(),
            extra_headers: Vec::new(),
        })
        .expect("genesis commit");

    let (_, tip) = (1..spec.commits.get())
        .try_fold(
            (genesis_tree, genesis_commit),
            |(prev_tree, prev_commit), revision| -> Result<(Oid, Oid), knot_git::GitError> {
                let revision = Revision::new(revision);
                let changes: Vec<StagedChange> = churn_indices(revision, spec)
                    .map(|index| put(index, revision))
                    .collect();
                let tree = repo.write_staged_tree(prev_tree, &changes)?;
                let commit = repo.write_commit(&NewCommit {
                    tree,
                    parents: vec![prev_commit],
                    author: identity(revision),
                    committer: identity(revision),
                    message: format!("revision {}", revision.get()),
                    extra_headers: Vec::new(),
                })?;
                Ok((tree, commit))
            },
        )
        .expect("commit chain");

    let main = RefName::new("refs/heads/main").expect("main ref name");
    repo.update_ref(&RefUpdate::Create {
        name: main.clone(),
        new: tip,
    })
    .expect("create main");
    repo.set_head(&main).expect("set head");

    tip
}

pub struct BuiltRefs {
    _dir: TempDir,
    repo: Repo,
}

impl BuiltRefs {
    pub fn repo(&self) -> &Repo {
        &self.repo
    }
}

pub fn build_many_refs(count: RefCount) -> BuiltRefs {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repo::create(dir.path().join("repo.git")).expect("create repo");
    let tip = write_history(
        &repo,
        HistorySpec {
            commits: CommitCount::new(1),
            paths: PathCount::new(1),
            churn: ChurnCount::new(1),
        },
    );
    (0..count.get()).for_each(|index| {
        let name = RefName::new(format!("refs/heads/branch{index:06}")).expect("ref name");
        repo.update_ref(&RefUpdate::Create { name, new: tip })
            .expect("create ref");
    });
    BuiltRefs { _dir: dir, repo }
}

fn synthetic_account(seed: u64) -> AccountDid {
    AccountDid::new(format!("did:plc:acct{seed:012}")).expect("account did")
}

fn synthetic_repo_did(index: u64) -> RepoDid {
    RepoDid::new(format!("did:plc:repo{index:012}")).expect("repo did")
}

fn registry_owner() -> OwnerDid {
    OwnerDid::new("did:plc:nel").expect("owner did")
}

fn knot_home() -> CobHome {
    CobHome::from(&KnotId::new("did:web:knot.nel.pet").expect("knot did"))
}

fn registration(index: u64) -> Registration {
    let rkey = format!("repo{index:012}");
    Registration {
        owner: registry_owner(),
        rkey: RepoRkey::new(&rkey).expect("rkey"),
        name: RepoName::new(&rkey).expect("repo name"),
        repo: synthetic_repo_did(index),
        created_at: UnixSeconds::new(GENESIS_SECONDS + index as i64),
    }
}

fn grant(seed: u64) -> Grant {
    Grant {
        subject: synthetic_account(seed),
        added_by: synthetic_account(0),
        created_at: UnixSeconds::new(GENESIS_SECONDS),
    }
}

pub struct BuiltRegistry {
    _dir: TempDir,
    meta_path: PathBuf,
    layout: Layout,
    dids: Vec<RepoDid>,
}

impl BuiltRegistry {
    pub fn index(&self) -> Index {
        Index::new(&self.meta_path, self.layout.clone())
    }
    pub fn dids(&self) -> &[RepoDid] {
        &self.dids
    }
    pub fn alias(&self, index: u64) -> (OwnerDid, RepoRkey) {
        let reg = registration(index);
        (reg.owner, reg.rkey)
    }
}

fn seed_members(meta_path: &Path, signer: &dyn knot_runtime::Signer) {
    let meta = Repo::open(meta_path).expect("open meta");
    let store = CobStore::new(&meta);
    store
        .create(
            &knot_home(),
            &knot_cobs::MembersChange::Add(grant(1)),
            signer,
            UnixSeconds::new(GENESIS_SECONDS),
        )
        .expect("seed members");
}

pub fn build_registry(repos: RepoCount) -> BuiltRegistry {
    let dir = tempfile::tempdir().expect("tempdir");
    let meta_path = dir.path().join("meta.git");
    Repo::create(&meta_path).expect("create meta");
    let layout = Layout::new(dir.path().join("repos"));
    let signer = K256Signer::generate(&SeededEntropy::new(7));

    seed_members(&meta_path, &signer);

    let meta = Repo::open(&meta_path).expect("open meta");
    let store = CobStore::new(&meta);
    let registry_object = store
        .create(
            &knot_home(),
            &RegistryChange::Register(registration(0)),
            &signer,
            UnixSeconds::new(GENESIS_SECONDS),
        )
        .expect("create registry")
        .object;
    (1..repos.get()).for_each(|index| {
        store
            .update(
                &knot_home(),
                registry_object,
                &RegistryChange::Register(registration(index)),
                &signer,
                UnixSeconds::new(GENESIS_SECONDS + index as i64),
            )
            .expect("register repo");
    });

    let dids: Vec<RepoDid> = (0..repos.get()).map(synthetic_repo_did).collect();

    dids.iter().enumerate().for_each(|(index, did)| {
        let git = layout.create(did).expect("create repo dir");
        let collab = CobStore::new(&git);
        collab
            .create(
                &CobHome::from(did),
                &CollaboratorsChange::Add(grant(index as u64 + 2)),
                &signer,
                UnixSeconds::new(GENESIS_SECONDS),
            )
            .expect("seed collaborator");
    });

    BuiltRegistry {
        _dir: dir,
        meta_path,
        layout,
        dids,
    }
}

pub struct BuiltRoster {
    _dir: TempDir,
    meta_path: PathBuf,
    layout: Layout,
    repo: RepoDid,
}

impl BuiltRoster {
    pub fn index(&self) -> Index {
        Index::new(&self.meta_path, self.layout.clone())
    }
    pub fn repo(&self) -> &RepoDid {
        &self.repo
    }
}

pub fn build_collaborator_roster(collaborators: RosterCount) -> BuiltRoster {
    let dir = tempfile::tempdir().expect("tempdir");
    let meta_path = dir.path().join("meta.git");
    Repo::create(&meta_path).expect("create meta");
    let layout = Layout::new(dir.path().join("repos"));
    let signer = K256Signer::generate(&SeededEntropy::new(9));

    seed_members(&meta_path, &signer);

    let repo = synthetic_repo_did(0);
    let git = layout.create(&repo).expect("create repo dir");
    let store = CobStore::new(&git);
    let home = CobHome::from(&repo);
    let object = store
        .create(
            &home,
            &CollaboratorsChange::Add(grant(2)),
            &signer,
            UnixSeconds::new(GENESIS_SECONDS),
        )
        .expect("seed collaborator")
        .object;
    (1..collaborators.get()).for_each(|index| {
        store
            .update(
                &home,
                object,
                &CollaboratorsChange::Add(grant(u64::from(index) + 2)),
                &signer,
                UnixSeconds::new(GENESIS_SECONDS + i64::from(index)),
            )
            .expect("add collaborator");
    });

    BuiltRoster {
        _dir: dir,
        meta_path,
        layout,
        repo,
    }
}

pub struct BuiltRegistryWriter {
    _dir: TempDir,
    meta_path: PathBuf,
    object: CobId,
    signer: K256Signer,
}

impl BuiltRegistryWriter {
    pub fn probe(&self) {
        let meta = Repo::open(&self.meta_path).expect("reopen meta");
        let store = CobStore::new(&meta);
        register_repo(
            &store,
            &knot_home(),
            self.object,
            registration(0),
            &self.signer,
            UnixSeconds::new(GENESIS_SECONDS),
        )
        .expect("idempotent re-register folds registry");
    }

    pub fn full_fold(&self) {
        let meta = Repo::open(&self.meta_path).expect("reopen meta");
        let store = CobStore::new(&meta);
        store
            .get::<RepoRegistryCob>(self.object)
            .expect("full fold of registry");
    }
}

pub fn build_registry_checkpointed(repos: RepoCount) -> BuiltRegistryWriter {
    let dir = tempfile::tempdir().expect("tempdir");
    let meta_path = dir.path().join("meta.git");
    Repo::create(&meta_path).expect("create meta");
    let signer = K256Signer::generate(&SeededEntropy::new(13));

    let meta = Repo::open(&meta_path).expect("open meta");
    let store = CobStore::new(&meta);
    let object = store
        .create(
            &knot_home(),
            &RegistryChange::Register(registration(0)),
            &signer,
            UnixSeconds::new(GENESIS_SECONDS),
        )
        .expect("create registry")
        .object;
    (1..repos.get()).for_each(|index| {
        register_repo(
            &store,
            &knot_home(),
            object,
            registration(index),
            &signer,
            UnixSeconds::new(GENESIS_SECONDS + index as i64),
        )
        .expect("register repo");
    });

    BuiltRegistryWriter {
        _dir: dir,
        meta_path,
        object,
        signer,
    }
}

pub struct BuiltMembersWriter {
    _dir: TempDir,
    meta_path: PathBuf,
    object: CobId,
    signer: K256Signer,
}

impl BuiltMembersWriter {
    pub fn probe(&self) {
        let meta = Repo::open(&self.meta_path).expect("reopen meta");
        let store = CobStore::new(&meta);
        store
            .update_maybe_checkpointed::<MembersCob, CobError>(
                &knot_home(),
                self.object,
                &self.signer,
                UnixSeconds::new(GENESIS_SECONDS),
                |roster| {
                    Ok(if roster.contains(&grant(0).subject) {
                        None
                    } else {
                        Some(MembersChange::Add(grant(0)))
                    })
                },
            )
            .expect("idempotent re-add folds the bounded suffix");
    }

    pub fn full_fold(&self) {
        let meta = Repo::open(&self.meta_path).expect("reopen meta");
        let store = CobStore::new(&meta);
        store
            .get::<MembersCob>(self.object)
            .expect("full fold of members");
    }
}

pub fn build_members_checkpointed(members: RosterCount) -> BuiltMembersWriter {
    let dir = tempfile::tempdir().expect("tempdir");
    let meta_path = dir.path().join("meta.git");
    Repo::create(&meta_path).expect("create meta");
    let signer = K256Signer::generate(&SeededEntropy::new(17));

    let meta = Repo::open(&meta_path).expect("open meta");
    let store = CobStore::new(&meta);
    let object = store
        .create(
            &knot_home(),
            &MembersChange::Add(grant(0)),
            &signer,
            UnixSeconds::new(GENESIS_SECONDS),
        )
        .expect("create members")
        .object;
    (1..members.get()).for_each(|index| {
        add_member(
            &store,
            &knot_home(),
            object,
            grant(u64::from(index) + 1),
            &signer,
            UnixSeconds::new(GENESIS_SECONDS + i64::from(index)),
        )
        .expect("add member");
    });

    BuiltMembersWriter {
        _dir: dir,
        meta_path,
        object,
        signer,
    }
}

pub struct BuiltLinearCob {
    _dir: TempDir,
    repo: Repo,
    object: CobId,
}

impl BuiltLinearCob {
    pub fn fold(&self) -> usize {
        let store = CobStore::new(&self.repo);
        store
            .get::<RepoRegistryCob>(self.object)
            .expect("fold registry")
            .state()
            .len()
    }
}

pub fn build_linear_cob(changes: ChangeCount) -> BuiltLinearCob {
    let dir = tempfile::tempdir().expect("tempdir");
    let meta_path = dir.path().join("meta.git");
    Repo::create(&meta_path).expect("create meta");
    let signer = K256Signer::generate(&SeededEntropy::new(11));

    let meta = Repo::open(&meta_path).expect("open meta");
    let store = CobStore::new(&meta);
    let object = store
        .create(
            &knot_home(),
            &RegistryChange::Register(registration(0)),
            &signer,
            UnixSeconds::new(GENESIS_SECONDS),
        )
        .expect("create registry")
        .object;
    (1..changes.get()).for_each(|index| {
        store
            .update(
                &knot_home(),
                object,
                &RegistryChange::Register(registration(u64::from(index))),
                &signer,
                UnixSeconds::new(GENESIS_SECONDS + i64::from(index)),
            )
            .expect("append change");
    });

    BuiltLinearCob {
        _dir: dir,
        repo: Repo::open(&meta_path).expect("reopen meta"),
        object,
    }
}
