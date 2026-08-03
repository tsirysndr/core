use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use knot_cob::{CobHome, CobStore};
use knot_cobs::{CollaboratorsChange, CollaboratorsCob, Grant, MembersChange};
use knot_git::Repo;
use knot_index::{Coverage, IndexCoverage, IndexError, OfferedKey, Resolved};

mod common;
use common::{World, acc, at, grant, meta_home, own, repo_did, rkey};

#[test]
fn rebuild_folds_members_and_registry_and_collaborators_fold_on_access() {
    let world = World::new();
    let repo = repo_did("squid");
    world.seed_members();
    world.seed_registry(&repo);
    world.seed_collaborator(&repo, "lyna");

    let index = world.index();
    index.rebuild().unwrap();

    assert_eq!(index.is_member(&acc("nel")), Resolved::Ready(true));
    assert_eq!(index.is_member(&acc("olaren")), Resolved::Ready(true));
    assert_eq!(index.is_member(&acc("teq")), Resolved::Ready(false));
    assert_eq!(
        index.resolve_repo(&own("nel"), &rkey("anemone")),
        Resolved::Ready(Some(repo.clone()))
    );
    assert_eq!(
        index.resolve_repo(&own("nel"), &rkey("nautilus")),
        Resolved::Ready(None)
    );
    assert_eq!(
        index.is_collaborator(&repo, &acc("lyna")),
        Resolved::Warming,
        "rebuild doesn't fold collaborators, so roster reads warming until first access"
    );

    index.ensure_collaborators(&repo).unwrap();
    assert_eq!(
        index.is_collaborator(&repo, &acc("lyna")),
        Resolved::Ready(true)
    );
    assert_eq!(
        index.is_collaborator(&repo, &acc("bailey")),
        Resolved::Ready(false)
    );
    assert_eq!(
        index.coverage(),
        IndexCoverage {
            members: Coverage::Ready,
            blocklist: Coverage::Ready,
            collaborators: Coverage::Ready,
            registry: Coverage::Ready,
            keys: Coverage::Warming,
        }
    );
}

#[test]
fn every_accessor_fails_closed_while_warming() {
    let world = World::new();
    world.seed_members();
    world.seed_collaborator(&repo_did("squid"), "lyna");
    let index = world.index();

    assert_eq!(index.is_member(&acc("nel")), Resolved::Warming);
    assert_eq!(index.member_entries(), Resolved::Warming);
    assert_eq!(
        index.is_collaborator(&repo_did("squid"), &acc("lyna")),
        Resolved::Warming
    );
    assert_eq!(
        index.collaborator_entries(&repo_did("squid")),
        Resolved::Warming,
        "roster that has never been folded fails closed"
    );
    assert_eq!(
        index.resolve_repo(&own("nel"), &rkey("anemone")),
        Resolved::Warming
    );
    assert_eq!(
        index.owner_of(&repo_did("squid")),
        Resolved::Warming,
        "repo lookup before rebuild fails closed"
    );
    assert_eq!(index.coverage().members, Coverage::Warming);

    assert_eq!(
        index.owner_of_key(&OfferedKey::from_bytes(vec![1, 2, 3]), at(0)),
        Resolved::Ready(None),
        "a key lookup answers from the first request, even while the key set is warming"
    );
}

#[test]
fn member_entries_record_provenance() {
    let world = World::new();
    world.seed_members();
    let index = world.index();
    index.rebuild().unwrap();
    assert_eq!(
        index.member_entries(),
        Resolved::Ready(vec![grant("nel", "nel", 1), grant("olaren", "nel", 2)])
    );
}

#[test]
fn a_re_added_member_keeps_the_first_provenance() {
    let world = World::new();
    let members = world.seed_members();
    let index = world.index();
    index.rebuild().unwrap();

    let meta = Repo::open(&world.meta_path).unwrap();
    let store = CobStore::new(&meta);
    store
        .update(
            &meta_home(),
            members,
            &MembersChange::Add(grant("olaren", "teq", 9)),
            &world.signer,
            at(9),
        )
        .unwrap();
    index.refresh_members().unwrap();

    assert_eq!(
        index.member_entries(),
        Resolved::Ready(vec![grant("nel", "nel", 1), grant("olaren", "nel", 2)]),
        "duplicate add never rewrites original provenance, matching canonical roster"
    );
}

#[test]
fn collaborator_entries_match_the_canonical_roster() {
    let world = World::new();
    let repo = repo_did("squid");
    world.seed_registry(&repo);
    let object = world.seed_collaborator(&repo, "lyna");
    let git = world.layout.open(&repo).unwrap();
    let store = CobStore::new(&git);
    store
        .update(
            &CobHome::from(&repo),
            object,
            &CollaboratorsChange::Add(grant("bailey", "olaren", 2)),
            &world.signer,
            at(2),
        )
        .unwrap();
    world.remove_collaborator(&repo, object, "lyna", 3);
    store
        .update(
            &CobHome::from(&repo),
            object,
            &CollaboratorsChange::Add(grant("lyna", "teq", 5)),
            &world.signer,
            at(5),
        )
        .unwrap();

    let index = world.index();
    index.rebuild().unwrap();
    index.ensure_collaborators(&repo).unwrap();

    let canonical = store.get::<CollaboratorsCob>(object).unwrap();
    let expected: Vec<Grant> = canonical
        .state()
        .entries()
        .map(|(subject, entry)| Grant {
            subject: subject.clone(),
            added_by: entry.added_by.clone(),
            created_at: entry.created_at,
        })
        .collect();
    assert_eq!(
        index.collaborator_entries(&repo),
        Resolved::Ready(expected),
        "projected entries disagree with canonical Evaluate fold"
    );
}

#[test]
fn a_folded_repo_serves_while_unaccessed_repos_stay_warming() {
    let world = World::new();
    let present = repo_did("squid");
    let absent = repo_did("kelp");
    let registry = world.seed_registry(&present);
    world.register_extra(&absent, "barnacle", registry);
    world.seed_collaborator(&present, "lyna");

    let index = world.index();
    index.rebuild().unwrap();
    assert_eq!(
        index.coverage().collaborators,
        Coverage::Ready,
        "collaborators projection is operational from boot"
    );
    index.ensure_collaborators(&present).unwrap();

    assert_eq!(
        index.is_collaborator(&present, &acc("lyna")),
        Resolved::Ready(true)
    );
    assert_eq!(
        index
            .collaborator_entries(&present)
            .map(|grants| grants.len()),
        Resolved::Ready(1),
        "folded repo serves its roster"
    );
    assert_eq!(
        index.collaborator_entries(&absent),
        Resolved::Warming,
        "registered repo that was never accessed stays fail-closed until folded"
    );
    assert_eq!(
        index.is_collaborator(&repo_did("conch"), &acc("lyna")),
        Resolved::Warming,
        "repo the index never folded cannot answer, so it fails closed"
    );
}

#[test]
fn an_ambiguous_meta_cob_fails_refresh_and_rebuild() {
    let world = World::new();
    let meta = Repo::open(&world.meta_path).unwrap();
    let store = CobStore::new(&meta);
    store
        .create(
            &meta_home(),
            &MembersChange::Add(grant("nel", "nel", 1)),
            &world.signer,
            at(1),
        )
        .unwrap();
    store
        .create(
            &meta_home(),
            &MembersChange::Add(grant("olaren", "olaren", 2)),
            &world.signer,
            at(2),
        )
        .unwrap();

    let index = world.index();
    assert!(matches!(
        index.refresh_members(),
        Err(IndexError::Ambiguous { count: 2, .. })
    ));
    assert!(
        matches!(index.rebuild(), Err(IndexError::Ambiguous { .. })),
        "broken meta COB fails whole boot instead of reporting partial one"
    );
}

#[test]
fn a_repo_missing_on_disk_does_not_fail_the_boot_and_isolates_its_fold() {
    let world = World::new();
    let present = repo_did("squid");
    let absent = repo_did("kelp");
    world.seed_members();
    let registry = world.seed_registry(&present);
    world.register_extra(&absent, "barnacle", registry);
    world.seed_collaborator(&present, "lyna");

    let index = world.index();
    index
        .rebuild()
        .expect("boot folds members and registry only, so missing repo dir never fails it");

    index.ensure_collaborators(&present).unwrap();
    assert_eq!(
        index.is_collaborator(&present, &acc("lyna")),
        Resolved::Ready(true)
    );
    assert!(
        index.ensure_collaborators(&absent).is_err(),
        "folding repo with no dir on disk fails for that repo alone"
    );
    assert_eq!(
        index.is_collaborator(&absent, &acc("lyna")),
        Resolved::Warming,
        "repo the index couldn't fold stays fail-closed"
    );
}

#[test]
fn concurrent_refreshes_of_distinct_repos_all_land() {
    let world = World::new();
    let repos = ["squid", "clam", "whelk", "conch"];
    repos.iter().for_each(|repo| {
        world.seed_collaborator(&repo_did(repo), "lyna");
    });

    let index = Arc::new(world.index());
    std::thread::scope(|scope| {
        repos.iter().for_each(|repo| {
            let index = Arc::clone(&index);
            let repo = repo_did(repo);
            scope.spawn(move || index.refresh_collaborators(&repo).unwrap());
        });
    });

    repos.iter().for_each(|repo| {
        assert_eq!(
            index.is_collaborator(&repo_did(repo), &acc("lyna")),
            Resolved::Ready(true)
        );
    });
}

#[test]
fn a_refresh_is_eventually_consistent_not_an_atomic_snapshot() {
    let world = World::new();
    let members = world.seed_members();
    let index = Arc::new(world.index());
    index.rebuild().unwrap();

    (0..32).for_each(|i| world.add_member(members, &format!("m{i}"), 10 + i as i64));

    let done = Arc::new(AtomicBool::new(false));
    std::thread::scope(|scope| {
        let writer = Arc::clone(&index);
        let writer_done = Arc::clone(&done);
        scope.spawn(move || {
            writer.refresh_members().unwrap();
            writer_done.store(true, Ordering::Release);
        });

        let reader = Arc::clone(&index);
        let reader_done = Arc::clone(&done);
        scope.spawn(move || {
            while !reader_done.load(Ordering::Acquire) {
                assert_eq!(
                    reader.is_member(&acc("nel")),
                    Resolved::Ready(true),
                    "stable member stays visible and read never blocks on writer"
                );
                assert!(
                    !reader.is_member(&acc("m0")).is_warming(),
                    "already-ready projection serves reads mid-refresh, it never re-warms"
                );
            }
        });
    });

    (0..32).for_each(|i| {
        assert_eq!(
            index.is_member(&acc(&format!("m{i}"))),
            Resolved::Ready(true),
            "once writer returns, whole delta has converged"
        );
    });
}
