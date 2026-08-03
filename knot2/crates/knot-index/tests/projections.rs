use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use knot_cob::{ChangePayload, CobHome, CobId, CobStore};
use knot_cobs::{CollaboratorsChange, MembersChange, RegistryChange, Removal, Rename, RepoRef};
use knot_git::{RefUpdate, Repo};
use knot_index::{Coverage, IndexError, Resolved};
use knot_types::{ClonePath, RefName, RepoName};
use serde::{Deserialize, Serialize};

mod common;
use common::{
    World, acc, at, grant, meta_home, named_registration, own, registration, repo_did, rkey,
};

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

fn path(raw: &str) -> ClonePath {
    ClonePath::parse(raw).unwrap()
}

#[test]
fn a_clone_path_resolves_by_name_when_the_record_key_is_a_tid() {
    let world = World::new();
    let repo = repo_did("squid");

    let meta = Repo::open(&world.meta_path).unwrap();
    let store = CobStore::new(&meta);
    store
        .create(
            &meta_home(),
            &RegistryChange::Register(named_registration(
                "nel",
                "3mizfnpxii522",
                "substratum.cloud",
                &repo,
                1,
            )),
            &world.signer,
            at(1),
        )
        .unwrap();

    let index = world.index();
    index.rebuild().unwrap();

    assert_eq!(
        index.resolve_clone_path(&own("nel"), &path("substratum.cloud")),
        Resolved::Ready(Some(repo.clone())),
        "a PDS-native record key leaves the display name as the only human clone path"
    );
    assert_eq!(
        index.resolve_clone_path(&own("nel"), &path("3mizfnpxii522")),
        Resolved::Ready(Some(repo.clone())),
        "the record key still resolves"
    );
    assert_eq!(
        index.resolve_clone_path(&own("nel"), &path("substratum.cloud.git")),
        Resolved::Ready(Some(repo)),
        "the conventional .git suffix strips before the name lookup"
    );
    assert_eq!(
        index.resolve_clone_path(&own("nel"), &path("periwinkle")),
        Resolved::Ready(None)
    );
}

#[test]
fn a_record_key_outranks_another_repos_name() {
    let world = World::new();
    let by_name = repo_did("limpet");
    let by_rkey = repo_did("mussel");

    let meta = Repo::open(&world.meta_path).unwrap();
    let store = CobStore::new(&meta);
    let object = store
        .create(
            &meta_home(),
            &RegistryChange::Register(named_registration("nel", "limpet", "mussel", &by_name, 1)),
            &world.signer,
            at(1),
        )
        .unwrap()
        .object;
    store
        .update(
            &meta_home(),
            object,
            &RegistryChange::Register(named_registration("nel", "mussel", "scallop", &by_rkey, 2)),
            &world.signer,
            at(2),
        )
        .unwrap();

    let index = world.index();
    index.rebuild().unwrap();

    assert_eq!(
        index.resolve_clone_path(&own("nel"), &path("mussel")),
        Resolved::Ready(Some(by_rkey)),
        "a record key match wins over another repo holding that string as its name"
    );
    assert_eq!(
        index.resolve_clone_path(&own("nel"), &path("scallop")),
        Resolved::Ready(Some(repo_did("mussel")))
    );
}

#[test]
fn a_name_two_repos_share_resolves_to_the_older_registration() {
    let world = World::new();
    let first = repo_did("whelk");
    let second = repo_did("conch");

    let meta = Repo::open(&world.meta_path).unwrap();
    let store = CobStore::new(&meta);
    let object = store
        .create(
            &meta_home(),
            &RegistryChange::Register(named_registration(
                "nel",
                "3lubrptx57d22",
                "kelp",
                &first,
                1,
            )),
            &world.signer,
            at(1),
        )
        .unwrap()
        .object;
    store
        .update(
            &meta_home(),
            object,
            &RegistryChange::Register(named_registration(
                "nel",
                "3mqydma3re27z",
                "kelp",
                &second,
                2,
            )),
            &world.signer,
            at(2),
        )
        .unwrap();

    let index = world.index();
    index.rebuild().unwrap();

    assert_eq!(
        index.resolve_clone_path(&own("nel"), &path("kelp")),
        Resolved::Ready(Some(first.clone())),
        "a contested name resolves to whichever repo registered first, ordered by \
         created_at so a cold seed and an incremental replay agree"
    );
    assert_eq!(
        index.resolve_clone_path(&own("nel"), &path("3lubrptx57d22")),
        Resolved::Ready(Some(first)),
        "each record key stays unambiguous"
    );
    assert_eq!(
        index.resolve_clone_path(&own("nel"), &path("3mqydma3re27z")),
        Resolved::Ready(Some(second)),
        "the repo that lost the name is still reachable by its record key"
    );
}

#[test]
fn a_repo_registered_later_never_takes_a_contested_name_by_renaming_onto_it() {
    let world = World::new();
    let holder = repo_did("whelk");
    let latecomer = repo_did("conch");

    let meta = Repo::open(&world.meta_path).unwrap();
    let store = CobStore::new(&meta);
    let object = store
        .create(
            &meta_home(),
            &RegistryChange::Register(named_registration(
                "nel",
                "3lubrptx57d22",
                "kelp",
                &holder,
                1,
            )),
            &world.signer,
            at(1),
        )
        .unwrap()
        .object;
    store
        .update(
            &meta_home(),
            object,
            &RegistryChange::Register(named_registration(
                "nel",
                "3mqydma3re27z",
                "uni",
                &latecomer,
                2,
            )),
            &world.signer,
            at(2),
        )
        .unwrap();

    let index = world.index();
    index.rebuild().unwrap();

    store
        .update(
            &meta_home(),
            object,
            &RegistryChange::Rename(Rename {
                owner: own("nel"),
                rkey: rkey("3mqydma3re27z"),
                name: RepoName::new("kelp").unwrap(),
                repo: latecomer.clone(),
            }),
            &world.signer,
            at(3),
        )
        .unwrap();
    index.refresh_registry().unwrap();

    assert_eq!(
        index.resolve_clone_path(&own("nel"), &path("kelp")),
        Resolved::Ready(Some(holder)),
        "a rename keeps the repo's original created_at, so renaming onto a name \
         another repo registered earlier cannot take it"
    );
    assert_eq!(
        index.resolve_clone_path(&own("nel"), &path("uni")),
        Resolved::Ready(None),
        "the renamed repo's previous name stops resolving"
    );
    assert_eq!(
        index.resolve_clone_path(&own("nel"), &path("3mqydma3re27z")),
        Resolved::Ready(Some(latecomer))
    );
}

#[test]
fn deregistering_the_older_registration_moves_a_shared_name_to_the_survivor() {
    let world = World::new();
    let first = repo_did("whelk");
    let survivor = repo_did("conch");

    let meta = Repo::open(&world.meta_path).unwrap();
    let store = CobStore::new(&meta);
    let object = store
        .create(
            &meta_home(),
            &RegistryChange::Register(named_registration(
                "nel",
                "3lubrptx57d22",
                "kelp",
                &first,
                1,
            )),
            &world.signer,
            at(1),
        )
        .unwrap()
        .object;
    store
        .update(
            &meta_home(),
            object,
            &RegistryChange::Register(named_registration(
                "nel",
                "3mqydma3re27z",
                "kelp",
                &survivor,
                2,
            )),
            &world.signer,
            at(2),
        )
        .unwrap();

    let index = world.index();
    index.rebuild().unwrap();
    assert_eq!(
        index.resolve_clone_path(&own("nel"), &path("kelp")),
        Resolved::Ready(Some(first)),
        "the name resolves to the older registration while both exist"
    );

    store
        .update(
            &meta_home(),
            object,
            &RegistryChange::Deregister(RepoRef {
                owner: own("nel"),
                rkey: rkey("3lubrptx57d22"),
            }),
            &world.signer,
            at(3),
        )
        .unwrap();
    index.refresh_registry().unwrap();

    assert_eq!(
        index.resolve_clone_path(&own("nel"), &path("kelp")),
        Resolved::Ready(Some(survivor)),
        "the name resolves to the remaining repo once the older one is deregistered"
    );
}

#[test]
fn a_rename_moves_name_resolution_off_the_old_name() {
    let world = World::new();
    let repo = repo_did("squid");

    let meta = Repo::open(&world.meta_path).unwrap();
    let store = CobStore::new(&meta);
    let object = store
        .create(
            &meta_home(),
            &RegistryChange::Register(named_registration(
                "nel",
                "3mizfnpxii522",
                "anemone",
                &repo,
                1,
            )),
            &world.signer,
            at(1),
        )
        .unwrap()
        .object;

    let index = world.index();
    index.rebuild().unwrap();
    assert_eq!(
        index.resolve_clone_path(&own("nel"), &path("anemone")),
        Resolved::Ready(Some(repo.clone()))
    );

    store
        .update(
            &meta_home(),
            object,
            &RegistryChange::Rename(Rename {
                owner: own("nel"),
                rkey: rkey("3mizfnpxii522"),
                name: RepoName::new("barnacle").unwrap(),
                repo: repo.clone(),
            }),
            &world.signer,
            at(2),
        )
        .unwrap();
    index.refresh_registry().unwrap();

    assert_eq!(
        index.resolve_clone_path(&own("nel"), &path("barnacle")),
        Resolved::Ready(Some(repo.clone())),
        "the new name resolves after a rename that keeps the record key"
    );
    assert_eq!(
        index.resolve_clone_path(&own("nel"), &path("anemone")),
        Resolved::Ready(None),
        "the superseded name stops resolving"
    );
    assert_eq!(
        index.resolve_clone_path(&own("nel"), &path("3mizfnpxii522")),
        Resolved::Ready(Some(repo))
    );
}

#[test]
fn a_rename_that_changes_only_the_record_key_keeps_the_name_resolving() {
    let world = World::new();
    let repo = repo_did("squid");

    let meta = Repo::open(&world.meta_path).unwrap();
    let store = CobStore::new(&meta);
    let object = store
        .create(
            &meta_home(),
            &RegistryChange::Register(named_registration("nel", "3lubrptx57d22", "kelp", &repo, 1)),
            &world.signer,
            at(1),
        )
        .unwrap()
        .object;

    let index = world.index();
    index.rebuild().unwrap();

    store
        .update(
            &meta_home(),
            object,
            &RegistryChange::Rename(Rename {
                owner: own("nel"),
                rkey: rkey("3mqydma3re27z"),
                name: RepoName::new("kelp").unwrap(),
                repo: repo.clone(),
            }),
            &world.signer,
            at(2),
        )
        .unwrap();
    index.refresh_registry().unwrap();

    assert_eq!(
        index.resolve_clone_path(&own("nel"), &path("kelp")),
        Resolved::Ready(Some(repo.clone())),
        "the unchanged name survives a rename that swaps the record key"
    );
    assert_eq!(
        index.resolve_clone_path(&own("nel"), &path("3mqydma3re27z")),
        Resolved::Ready(Some(repo.clone()))
    );
    assert_eq!(
        index.resolve_clone_path(&own("nel"), &path("3lubrptx57d22")),
        Resolved::Ready(Some(repo)),
        "the superseded record key keeps resolving through its retained alias"
    );
}

#[test]
fn seeding_and_replaying_agree_on_name_resolution() {
    let world = World::new();
    let first = repo_did("whelk");
    let second = repo_did("conch");

    let meta = Repo::open(&world.meta_path).unwrap();
    let store = CobStore::new(&meta);
    let object = store
        .create(
            &meta_home(),
            &RegistryChange::Register(named_registration(
                "nel",
                "3lubrptx57d22",
                "kelp",
                &first,
                1,
            )),
            &world.signer,
            at(1),
        )
        .unwrap()
        .object;

    let replayed = world.index();
    replayed.rebuild().unwrap();

    store
        .update(
            &meta_home(),
            object,
            &RegistryChange::Register(named_registration(
                "nel",
                "3mqydma3re27z",
                "uni",
                &second,
                2,
            )),
            &world.signer,
            at(2),
        )
        .unwrap();
    replayed.refresh_registry().unwrap();

    let seeded = world.index();
    seeded.rebuild().unwrap();

    ["kelp", "uni", "3lubrptx57d22", "3mqydma3re27z", "nautilus"]
        .into_iter()
        .for_each(|segment| {
            assert_eq!(
                replayed.resolve_clone_path(&own("nel"), &path(segment)),
                seeded.resolve_clone_path(&own("nel"), &path(segment)),
                "incremental replay and a cold seed disagree on {segment}"
            );
        });
}

#[test]
fn a_name_differing_from_its_record_key_only_by_case_resolves() {
    let world = World::new();
    let repo = repo_did("squid");

    let meta = Repo::open(&world.meta_path).unwrap();
    let store = CobStore::new(&meta);
    store
        .create(
            &meta_home(),
            &RegistryChange::Register(named_registration(
                "nel",
                "runic_lang",
                "Runic_lang",
                &repo,
                1,
            )),
            &world.signer,
            at(1),
        )
        .unwrap();

    let index = world.index();
    index.rebuild().unwrap();

    assert_eq!(
        index.resolve_clone_path(&own("nel"), &path("Runic_lang")),
        Resolved::Ready(Some(repo.clone())),
        "the appview lowercases the record key but keeps the display name's case, \
         so the mixed-case path resolves by name"
    );
    assert_eq!(
        index.resolve_clone_path(&own("nel"), &path("runic_lang")),
        Resolved::Ready(Some(repo)),
        "the lowercased record key still resolves"
    );
}
