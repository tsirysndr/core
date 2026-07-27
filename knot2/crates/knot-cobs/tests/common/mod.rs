#![allow(dead_code)]

use knot_cob::{ChangePayload, CobError, CobHome, CobId, CobStore};
use knot_cobs::{Grant, Members, MembersChange, MembersCob, Registration, RegistryChange, Rename};
use knot_git::{Layout, RefUpdate, Repo};
use knot_runtime::{K256Signer, SeededEntropy, Signer};
use knot_types::{
    AccountDid, ActorId, Oid, OwnerDid, RefName, RepoDid, RepoName, RepoRkey, TypeName, UnixSeconds,
};
use tempfile::TempDir;

pub fn fixture() -> (TempDir, Repo) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Layout::new(dir.path())
        .create(&RepoDid::new("did:plc:squid").unwrap())
        .unwrap();
    (dir, repo)
}

pub fn signer(seed: u64) -> K256Signer {
    K256Signer::generate(&SeededEntropy::new(seed))
}

pub fn home() -> CobHome {
    CobHome::from(&RepoDid::new("did:plc:squid").unwrap())
}

pub fn account(suffix: &str) -> AccountDid {
    AccountDid::new(format!("did:plc:{suffix}")).unwrap()
}

pub fn grant(subject: &str, added_by: &str, at: i64) -> Grant {
    Grant {
        subject: account(subject),
        added_by: account(added_by),
        created_at: UnixSeconds::new(at),
    }
}

pub fn at(seconds: i64) -> UnixSeconds {
    UnixSeconds::new(seconds)
}

pub fn owner_of(seed: u64) -> ActorId {
    ActorId::from_secp256k1(signer(seed).public_key().as_bytes())
}

pub fn rkey(value: &str) -> RepoRkey {
    RepoRkey::new(value).unwrap()
}

pub fn did<T: std::str::FromStr>(suffix: &str) -> T
where
    T::Err: std::fmt::Debug,
{
    format!("did:plc:{suffix}").parse().unwrap()
}

pub fn registration(owner: &str, key: &str, repo_id: &str, ts: i64) -> Registration {
    Registration {
        owner: OwnerDid::new(format!("did:plc:{owner}")).unwrap(),
        rkey: rkey(key),
        name: RepoName::new(key).unwrap(),
        repo: RepoDid::new(format!("did:plc:{repo_id}")).unwrap(),
        created_at: at(ts),
    }
}

pub fn rename(owner_id: &str, key: &str, repo_id: &str) -> Rename {
    Rename {
        owner: did::<OwnerDid>(owner_id),
        rkey: rkey(key),
        name: RepoName::new(key).unwrap(),
        repo: did::<RepoDid>(repo_id),
    }
}

pub fn reopen(repo: Repo) -> Repo {
    let path = repo.path().to_path_buf();
    drop(repo);
    Repo::open(path).unwrap()
}

pub fn members_store(
    seed: u64,
    steps: &[(MembersChange, i64)],
) -> (TempDir, Repo, K256Signer, CobId) {
    let (dir, repo) = fixture();
    let key = signer(seed);
    let store = CobStore::new(&repo);
    let (first, first_at) = &steps[0];
    let created = store.create(&home(), first, &key, at(*first_at)).unwrap();
    steps[1..].iter().for_each(|(change, ts)| {
        store
            .update(&home(), created.object, change, &key, at(*ts))
            .unwrap();
    });
    (dir, repo, key, created.object)
}

pub fn build_members(seed: u64, steps: &[(MembersChange, i64)]) -> Members {
    let (_dir, repo, _key, object) = members_store(seed, steps);
    CobStore::new(&repo)
        .get::<MembersCob>(object)
        .unwrap()
        .into_state()
}

pub fn write_cob_commit(
    repo: &Repo,
    type_name: &TypeName,
    payload: &[u8],
    parents: &[Oid],
    author: &ActorId,
    timestamp: i64,
) -> Oid {
    let git = repo.git();
    let payload_oid = git.write_blob(payload).unwrap().detach();
    let tree = gix::objs::Tree {
        entries: vec![gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Blob.into(),
            filename: "payload".into(),
            oid: payload_oid,
        }],
    };
    let revision = git.write_object(tree).unwrap().detach();
    let identity = gix::actor::Signature {
        name: "knot".into(),
        email: "noreply@knot".into(),
        time: gix::date::Time::new(timestamp, 0),
    };
    let commit = gix::objs::Commit {
        tree: revision,
        parents: parents.iter().map(|oid| oid.object_id()).collect(),
        author: identity.clone(),
        committer: identity,
        encoding: None,
        message: Vec::new().into(),
        extra_headers: vec![
            ("cob-type".into(), type_name.as_str().into()),
            ("cob-author".into(), author.as_str().into()),
            ("cob-sig".into(), "00".into()),
        ],
    };
    Oid::from(git.write_object(commit).unwrap().detach())
}

pub fn cob_ref(type_name: &TypeName, object: CobId) -> RefName {
    RefName::new(format!(
        "refs/cobs/{}/{}",
        type_name.as_str(),
        object.oid().to_hex()
    ))
    .unwrap()
}

pub fn forked_members_object(
    seed: u64,
    root: (MembersChange, i64),
    left: (MembersChange, i64),
    right: (MembersChange, i64),
    merge: (MembersChange, i64),
) -> (TempDir, Repo, CobId) {
    let (dir, repo) = fixture();
    let nsid = MembersChange::type_name();
    let author = ActorId::from_secp256k1(signer(seed).public_key().as_bytes());
    let enc = |c: &MembersChange| c.encode().unwrap();

    let root_oid = write_cob_commit(&repo, &nsid, &enc(&root.0), &[], &author, root.1);
    let object = CobId::new(root_oid);
    let left_oid = write_cob_commit(&repo, &nsid, &enc(&left.0), &[root_oid], &author, left.1);
    let right_oid = write_cob_commit(&repo, &nsid, &enc(&right.0), &[root_oid], &author, right.1);
    let merge_oid = write_cob_commit(
        &repo,
        &nsid,
        &enc(&merge.0),
        &[left_oid, right_oid],
        &author,
        merge.1,
    );
    repo.update_ref(&RefUpdate::Create {
        name: cob_ref(&nsid, object),
        new: merge_oid,
    })
    .unwrap();
    (dir, repo, object)
}

pub fn forked_members(
    seed: u64,
    root: (MembersChange, i64),
    left: (MembersChange, i64),
    right: (MembersChange, i64),
    merge: (MembersChange, i64),
) -> Result<Members, CobError> {
    let (_dir, repo, object) = forked_members_object(seed, root, left, right, merge);
    CobStore::new(&repo)
        .get::<MembersCob>(object)
        .map(|object| object.into_state())
}

pub fn registry_with(repo: &Repo, key: &K256Signer, name: &str, repo_id: &str) -> CobId {
    let store = CobStore::new(repo);
    store
        .create(
            &home(),
            &RegistryChange::Register(registration("nel", name, repo_id, 1)),
            key,
            at(1),
        )
        .unwrap()
        .object
}
