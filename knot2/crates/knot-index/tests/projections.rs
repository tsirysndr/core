use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use knot_cob::{ChangePayload, CobHome, CobId, CobStore};
use knot_cobs::{CollaboratorsChange, MembersChange, RegistryChange, Removal, Rename, RepoRef};
use knot_git::{RefUpdate, Repo};
use knot_index::{Coverage, IndexError, OfferedKey, Resolved};
use knot_types::{RefName, RepoName};
use serde::{Deserialize, Serialize};

mod common;
use common::{World, acc, at, grant, meta_home, own, registration, repo_did, rkey};

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", content = "data", rename_all = "snake_case")]
enum BadMembers {
    Explode(u8),
}
impl ChangePayload for BadMembers {
    const TYPE: &'static str = "sh.tangled.knot.member";
}

#[test]
fn a_concurrent_reader_never_sees_a_net_absent_subject() {
    let world = World::new();
    let object = world.seed_members();
    let index = Arc::new(world.index());
    index.rebuild().unwrap();
    assert_eq!(index.is_member(&acc("teq")), Resolved::Ready(false));

    let meta = Repo::open(&world.meta_path).unwrap();
    let store = CobStore::new(&meta);
    store
        .update(
            &meta_home(),
            object,
            &MembersChange::Add(grant("teq", "nel", 10)),
            &world.signer,
            at(10),
        )
        .unwrap();
    (0..1000).for_each(|i| {
        store
            .update(
                &meta_home(),
                object,
                &MembersChange::Add(grant(&format!("f{i}"), "nel", 11 + i)),
                &world.signer,
                at(11 + i),
            )
            .unwrap();
    });
    store
        .update(
            &meta_home(),
            object,
            &MembersChange::Remove(Removal {
                subject: acc("teq"),
            }),
            &world.signer,
            at(20_000),
        )
        .unwrap();

    let done = Arc::new(AtomicBool::new(false));
    let leaked = Arc::new(AtomicBool::new(false));
    std::thread::scope(|scope| {
        let reader = Arc::clone(&index);
        let reader_done = Arc::clone(&done);
        let reader_leaked = Arc::clone(&leaked);
        scope.spawn(move || {
            while !reader_done.load(Ordering::Acquire) {
                if reader.is_member(&acc("teq")) == Resolved::Ready(true) {
                    reader_leaked.store(true, Ordering::Release);
                }
            }
        });
        index.refresh_members().unwrap();
        done.store(true, Ordering::Release);
    });

    assert!(
        !leaked.load(Ordering::Acquire),
        "net-absent subject is never written, so no reader can observe it mid-delta"
    );
    assert_eq!(index.is_member(&acc("teq")), Resolved::Ready(false));
    assert_eq!(index.is_member(&acc("f0")), Resolved::Ready(true));
    assert_eq!(index.is_member(&acc("f999")), Resolved::Ready(true));
}

#[test]
fn a_concurrent_reader_never_sees_a_collaborator_roster_emptied_mid_refresh() {
    let world = World::new();
    let repo = repo_did("squid");
    let git = world.layout.create(&repo).unwrap();
    let store = CobStore::new(&git);
    let object = store
        .create(
            &CobHome::from(&repo),
            &CollaboratorsChange::Add(grant("anchor", "nel", 1)),
            &world.signer,
            at(1),
        )
        .unwrap()
        .object;
    (0..128).for_each(|i| {
        store
            .update(
                &CobHome::from(&repo),
                object,
                &CollaboratorsChange::Add(grant(&format!("c{i}"), "nel", 2 + i)),
                &world.signer,
                at(2 + i),
            )
            .unwrap();
    });

    let index = Arc::new(world.index());
    index.rebuild().unwrap();
    index.ensure_collaborators(&repo).unwrap();
    assert_eq!(
        index.is_collaborator(&repo, &acc("anchor")),
        Resolved::Ready(true)
    );

    let done = Arc::new(AtomicBool::new(false));
    let leaked = Arc::new(AtomicBool::new(false));
    std::thread::scope(|scope| {
        let reader = Arc::clone(&index);
        let reader_done = Arc::clone(&done);
        let reader_leaked = Arc::clone(&leaked);
        let target = repo.clone();
        scope.spawn(move || {
            while !reader_done.load(Ordering::Acquire) {
                if reader.is_collaborator(&target, &acc("anchor")) != Resolved::Ready(true) {
                    reader_leaked.store(true, Ordering::Release);
                }
            }
        });
        (0..500).for_each(|_| index.refresh_collaborators(&repo).unwrap());
        done.store(true, Ordering::Release);
    });

    assert!(
        !leaked.load(Ordering::Acquire),
        "in-place mem::take runs under per-repo lock, so anchor present in both \
         pre- and post-refresh roster is never observed absent or warming mid-refresh"
    );
    assert_eq!(
        index.is_collaborator(&repo, &acc("anchor")),
        Resolved::Ready(true)
    );
}

fn cob_ref(type_name: &str, object: CobId) -> RefName {
    RefName::new(format!("refs/cobs/{type_name}/{}", object.oid())).unwrap()
}

#[test]
fn a_diverged_collaborators_tip_purges_the_roster_instead_of_serving_it_stale() {
    let world = World::new();
    let repo = repo_did("squid");
    let git = world.layout.create(&repo).unwrap();
    let store = CobStore::new(&git);
    let created = store
        .create(
            &CobHome::from(&repo),
            &CollaboratorsChange::Add(grant("lyna", "nel", 1)),
            &world.signer,
            at(1),
        )
        .unwrap();
    let tip = store
        .update(
            &CobHome::from(&repo),
            created.object,
            &CollaboratorsChange::Add(grant("bailey", "nel", 2)),
            &world.signer,
            at(2),
        )
        .unwrap();

    let index = world.index();
    index.rebuild().unwrap();
    index.ensure_collaborators(&repo).unwrap();
    assert_eq!(
        index.is_collaborator(&repo, &acc("lyna")),
        Resolved::Ready(true)
    );

    git.update_ref(&RefUpdate::Update {
        name: cob_ref(CollaboratorsChange::TYPE, created.object),
        old: tip.oid(),
        new: created.object.oid(),
    })
    .unwrap();

    assert!(
        index.refresh_collaborators(&repo).is_err(),
        "tip that no longer descends from folded tip is structural error"
    );
    assert_eq!(
        index.is_collaborator(&repo, &acc("lyna")),
        Resolved::Warming,
        "diverged COB tip purges roster and fails closed, it does not serve \
         pre-divergence collaborators"
    );
}

#[test]
fn a_diverged_members_tip_fails_closed_to_warming() {
    let world = World::new();
    let object = world.seed_members();
    let meta = Repo::open(&world.meta_path).unwrap();
    let store = CobStore::new(&meta);
    let tip = store
        .update(
            &meta_home(),
            object,
            &MembersChange::Add(grant("teq", "nel", 3)),
            &world.signer,
            at(3),
        )
        .unwrap();

    let index = world.index();
    index.rebuild().unwrap();
    assert_eq!(index.is_member(&acc("nel")), Resolved::Ready(true));

    meta.update_ref(&RefUpdate::Update {
        name: cob_ref(MembersChange::TYPE, object),
        old: tip.oid(),
        new: object.oid(),
    })
    .unwrap();

    assert!(
        index.refresh_members().is_err(),
        "tip that no longer descends from folded tip is structural error"
    );
    assert_eq!(index.coverage().members, Coverage::Warming);
    assert_eq!(
        index.is_member(&acc("nel")),
        Resolved::Warming,
        "diverged members COB fails projection closed instead of serving stale members"
    );
}

#[test]
fn an_undecodable_change_fails_closed_with_no_partial_apply() {
    let world = World::new();
    let object = world.seed_members();
    let index = world.index();
    index.rebuild().unwrap();

    let meta = Repo::open(&world.meta_path).unwrap();
    let store = CobStore::new(&meta);
    store
        .update(
            &meta_home(),
            object,
            &MembersChange::Add(grant("teq", "nel", 3)),
            &world.signer,
            at(3),
        )
        .unwrap();
    store
        .update(
            &meta_home(),
            object,
            &BadMembers::Explode(0),
            &world.signer,
            at(4),
        )
        .unwrap();
    store
        .update(
            &meta_home(),
            object,
            &MembersChange::Remove(Removal {
                subject: acc("teq"),
            }),
            &world.signer,
            at(5),
        )
        .unwrap();

    assert!(matches!(
        index.refresh_members(),
        Err(IndexError::Decode { .. })
    ));

    assert_eq!(index.coverage().members, Coverage::Warming);
    assert_eq!(
        index.is_member(&acc("teq")),
        Resolved::Warming,
        "no partial apply: teq from pre-error change was never committed"
    );
    assert_eq!(
        index.is_member(&acc("nel")),
        Resolved::Warming,
        "structurally broken COB fails whole projection closed"
    );

    assert!(matches!(
        index.refresh_members(),
        Err(IndexError::Decode { .. })
    ));
    assert_eq!(index.coverage().members, Coverage::Warming);
}

#[test]
fn deregister_purges_collaborators_fail_closed() {
    let world = World::new();
    let repo = repo_did("clam");

    let meta = Repo::open(&world.meta_path).unwrap();
    let store = CobStore::new(&meta);
    let registry = store
        .create(
            &meta_home(),
            &RegistryChange::Register(registration("nel", "anemone", &repo, 1)),
            &world.signer,
            at(1),
        )
        .unwrap()
        .object;

    let git = world.layout.create(&repo).unwrap();
    let cstore = CobStore::new(&git);
    cstore
        .create(
            &CobHome::from(&repo),
            &CollaboratorsChange::Add(grant("lyna", "nel", 1)),
            &world.signer,
            at(1),
        )
        .unwrap();

    let index = world.index();
    index.rebuild().unwrap();
    index.warm_collaborators();
    assert_eq!(
        index.is_collaborator(&repo, &acc("lyna")),
        Resolved::Ready(true)
    );

    store
        .update(
            &meta_home(),
            registry,
            &RegistryChange::Deregister(RepoRef {
                owner: own("nel"),
                rkey: rkey("anemone"),
            }),
            &world.signer,
            at(2),
        )
        .unwrap();
    index.refresh_registry().unwrap();

    assert_eq!(
        index.resolve_repo(&own("nel"), &rkey("anemone")),
        Resolved::Ready(None)
    );
    assert_eq!(
        index.is_collaborator(&repo, &acc("lyna")),
        Resolved::Warming,
        "deregistered repo's collaborators are purged and fail closed, not served stale"
    );
}

#[test]
fn a_renamed_repo_keeps_both_rkeys_and_its_collaborators() {
    let world = World::new();
    let repo = repo_did("squid");

    let meta = Repo::open(&world.meta_path).unwrap();
    let store = CobStore::new(&meta);
    let registry = store
        .create(
            &meta_home(),
            &RegistryChange::Register(registration("nel", "anemone", &repo, 1)),
            &world.signer,
            at(1),
        )
        .unwrap()
        .object;

    let git = world.layout.create(&repo).unwrap();
    CobStore::new(&git)
        .create(
            &CobHome::from(&repo),
            &CollaboratorsChange::Add(grant("lyna", "nel", 1)),
            &world.signer,
            at(1),
        )
        .unwrap();

    let index = world.index();
    index.rebuild().unwrap();
    index.warm_collaborators();
    assert_eq!(index.rkey_of(&repo), Resolved::Ready(Some(rkey("anemone"))));

    store
        .update(
            &meta_home(),
            registry,
            &RegistryChange::Rename(Rename {
                owner: own("nel"),
                rkey: rkey("barnacle"),
                name: RepoName::new("barnacle").unwrap(),
                repo: repo.clone(),
            }),
            &world.signer,
            at(2),
        )
        .unwrap();
    index.refresh_registry().unwrap();

    assert_eq!(
        index.resolve_repo(&own("nel"), &rkey("anemone")),
        Resolved::Ready(Some(repo.clone())),
        "prior rkey keeps resolving as alias after rename is delta-applied"
    );
    assert_eq!(
        index.resolve_repo(&own("nel"), &rkey("barnacle")),
        Resolved::Ready(Some(repo.clone()))
    );
    assert_eq!(
        index.rkey_of(&repo),
        Resolved::Ready(Some(rkey("barnacle"))),
        "new rkey is canonical"
    );
    assert_eq!(
        index.is_collaborator(&repo, &acc("lyna")),
        Resolved::Ready(true),
        "rename never evacuates repo, so its collaborators survive"
    );

    store
        .update(
            &meta_home(),
            registry,
            &RegistryChange::Deregister(RepoRef {
                owner: own("nel"),
                rkey: rkey("anemone"),
            }),
            &world.signer,
            at(3),
        )
        .unwrap();
    index.refresh_registry().unwrap();
    assert_eq!(
        index.resolve_repo(&own("nel"), &rkey("barnacle")),
        Resolved::Ready(None),
        "deregistering through retained alias removes repo and every alias"
    );
    assert_eq!(
        index.is_collaborator(&repo, &acc("lyna")),
        Resolved::Warming,
        "deregistered repo's collaborators are evacuated"
    );
}

#[test]
fn a_repo_moved_within_one_delta_is_not_evacuated() {
    let world = World::new();
    let repo = repo_did("squid");

    let meta = Repo::open(&world.meta_path).unwrap();
    let store = CobStore::new(&meta);
    let registry = store
        .create(
            &meta_home(),
            &RegistryChange::Register(registration("nel", "anemone", &repo, 1)),
            &world.signer,
            at(1),
        )
        .unwrap()
        .object;

    let git = world.layout.create(&repo).unwrap();
    CobStore::new(&git)
        .create(
            &CobHome::from(&repo),
            &CollaboratorsChange::Add(grant("lyna", "nel", 1)),
            &world.signer,
            at(1),
        )
        .unwrap();

    let index = world.index();
    index.rebuild().unwrap();
    index.warm_collaborators();
    assert_eq!(
        index.is_collaborator(&repo, &acc("lyna")),
        Resolved::Ready(true)
    );

    store
        .update(
            &meta_home(),
            registry,
            &RegistryChange::Deregister(RepoRef {
                owner: own("nel"),
                rkey: rkey("anemone"),
            }),
            &world.signer,
            at(2),
        )
        .unwrap();
    store
        .update(
            &meta_home(),
            registry,
            &RegistryChange::Register(registration("nel", "barnacle", &repo, 3)),
            &world.signer,
            at(3),
        )
        .unwrap();
    index.refresh_registry().unwrap();

    assert_eq!(
        index.resolve_repo(&own("nel"), &rkey("anemone")),
        Resolved::Ready(None)
    );
    assert_eq!(
        index.resolve_repo(&own("nel"), &rkey("barnacle")),
        Resolved::Ready(Some(repo.clone()))
    );
    assert_eq!(
        index.is_collaborator(&repo, &acc("lyna")),
        Resolved::Ready(true),
        "deregister and re-register within single delta leaves repo hosted, so its collaborators survive"
    );
}

#[test]
fn key_cache_evicts_least_recently_used() {
    const CAP: u32 = 16_384;
    let world = World::new();
    let index = world.index();
    let key = |i: u32| OfferedKey::from_bytes(i.to_le_bytes().to_vec());

    (0..CAP).for_each(|i| index.cache_key(key(i), &acc("nel")));
    assert_eq!(
        index.owner_of_key(&key(0)),
        Resolved::Ready(Some(acc("nel")))
    );
    index.cache_key(key(CAP), &acc("nel"));

    assert_eq!(
        index.owner_of_key(&key(1)),
        Resolved::Ready(None),
        "least-recently-used key is evicted"
    );
    assert_eq!(
        index.owner_of_key(&key(0)),
        Resolved::Ready(Some(acc("nel"))),
        "recently-used key survives despite being inserted first"
    );
    assert_eq!(
        index.owner_of_key(&key(CAP)),
        Resolved::Ready(Some(acc("nel")))
    );
}
