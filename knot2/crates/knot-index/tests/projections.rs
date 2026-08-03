use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use knot_cob::{ChangePayload, CobHome, CobId, CobStore};
use knot_cobs::{CollaboratorsChange, MembersChange, RegistryChange, Removal, Rename, RepoRef};
use knot_git::{RefUpdate, Repo};
use knot_index::{
    Coverage, HostedCoverage, IndexError, KeptAccounts, KeyBudget, KeyRecord, KeyReprieve,
    KeyReprieved, KeyTtl, OfferedKey, Resolved, SweepFloor,
};
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

#[test]
fn recording_and_retaining_track_who_still_publishes_each_key() {
    let world = World::new();
    let index = world.index();
    let key = |i: u32| OfferedKey::from_bytes(i.to_le_bytes().to_vec());

    index
        .keys()
        .record(&acc("nel"), vec![key(0), key(1)], hour(0));
    index
        .keys()
        .record(&acc("nel"), vec![key(1), key(2)], hour(0));
    assert_eq!(
        index.owner_of_key(&key(0), at(0)),
        Resolved::Ready(None),
        "a key the account stopped publishing stops resolving to it"
    );
    assert_eq!(
        index.owner_of_key(&key(1), at(0)),
        Resolved::Ready(Some(acc("nel"))),
        "a key present in both sets survives the replacement"
    );

    index
        .keys()
        .record(&acc("cuttle"), vec![key(1), key(3)], hour(0));
    assert_eq!(
        index
            .keys()
            .publisher_among(&[acc("cuttle")], &key(1), at(0)),
        Some(acc("cuttle")),
        "a deploy key two accounts publish must answer for the candidate that may push here, \
         whichever of them the knot happened to read first"
    );
    assert_eq!(
        index.keys().publisher_among(&[acc("teq")], &key(1), at(0)),
        None,
        "the key mustn't answer for an account that doesn't publish it"
    );

    index.keys().retain(&KeptAccounts::new(vec![acc("nel")]));
    assert_eq!(
        index.owner_of_key(&key(3), at(0)),
        Resolved::Ready(None),
        "an account outside the grant set has its keys released"
    );
    assert_eq!(
        index.owner_of_key(&key(1), at(0)),
        Resolved::Ready(Some(acc("nel"))),
        "one account leaving the grant set mustn't take a shared deploy key away from the \
         account that still publishes it"
    );

    index.keys().record(&acc("nel"), vec![key(2)], hour(0));
    assert_eq!(
        index.owner_of_key(&key(1), at(0)),
        Resolved::Ready(None),
        "with its last publisher no longer offering it, the key stops answering for anybody"
    );
}

#[test]
fn a_full_budget_reports_unheld_readings_and_then_refuses_to_grow_at_all() {
    let world = World::new();
    let index = world.index_within(KeyBudget::from_bytes(560));
    let fat = |seed: u8| OfferedKey::from_bytes(vec![seed; 48]);

    assert_eq!(
        index.keys().record(&acc("nel"), vec![fat(1)], hour(0)),
        KeyRecord::Stored
    );
    assert!(
        !index.keys().any_unheld(),
        "a set that kept every reading it took can vouch for its misses"
    );

    assert_eq!(
        index.keys().record(&acc("cuttle"), vec![fat(2)], hour(0)),
        KeyRecord::Unheld,
        "a budget too small for another account's keys still has room to record that the \
         knot read the account"
    );
    assert!(
        index.keys().any_unheld(),
        "a reading the budget couldn't fit means an unknown key may still belong to a \
         candidate, so the handshake must defer to the push check instead of refusing"
    );
    assert!(
        !index.keys().is_fresh(&acc("cuttle"), at(0)),
        "the refused account stays stale, so the fill keeps reporting the set incomplete and \
         the knot keeps checking its pushes against its PDS"
    );
    assert_eq!(
        index.owner_of_key(&fat(2), at(0)),
        Resolved::Ready(None),
        "a key the budget refused mustn't answer for anybody"
    );

    assert_eq!(
        index.keys().record(&acc("teq"), vec![fat(3)], hour(0)),
        KeyRecord::Saturated,
        "a knot whose grant set publishes more key bytes than it budgeted for must stop growing"
    );
    assert_eq!(
        index.owner_of_key(&fat(1), at(0)),
        Resolved::Ready(Some(acc("nel"))),
        "refusing the accounts that didn't fit mustn't release the accounts already on file"
    );

    index.keys().retain(&KeptAccounts::new(vec![acc("cuttle")]));
    assert!(
        index.keys().any_unheld(),
        "releasing a different account mustn't clear the report while the unheld reading stays"
    );
    assert_eq!(
        index.keys().record(&acc("cuttle"), vec![fat(2)], hour(0)),
        KeyRecord::Stored,
        "the budget frees up with the accounts that leave the grant set"
    );
    assert!(
        !index.keys().any_unheld(),
        "with every reading back on file the handshake can refuse unknown keys outright"
    );

    assert_eq!(
        index.keys().record(&acc("teq"), vec![fat(3)], hour(0)),
        KeyRecord::Unheld
    );
    assert_eq!(
        index
            .keys()
            .reprieve(&acc("teq"), at(21_601), grace(), hour(21_601)),
        KeyReprieved::Exhausted
    );
    assert!(
        !index.keys().any_unheld(),
        "an account the knot gave up rereading is recorded with an empty key set, so its \
         unheld reading mustn't keep the handshake open"
    );
}

#[test]
fn a_pass_renews_at_the_lease_halfway_and_rereads_once_per_miss_outside_the_floor() {
    let (_world, index) = folded();
    index
        .keys()
        .record(&acc("nel"), vec![OfferedKey::from_bytes(vec![7])], hour(0));

    assert!(
        matches!(index.keys().work(at(0), sweep()), Resolved::Ready(work)
            if work.due.is_empty() && work.suspected.is_empty()),
        "with every account's keys read inside the ttl the fill won't fetch anything"
    );

    index.keys().note_miss();
    let Resolved::Ready(work) = index.keys().work(at(100), sweep()) else {
        panic!("the registry is folded, so the knot knows who may push");
    };
    assert_eq!(
        work.suspected.len(),
        1,
        "a key the accounts on file don't publish is the only evidence the knot gets that an \
         account published a new key, so the next pass must reread rather than wait out the ttl"
    );
    assert!(
        work.due.is_empty(),
        "a reread a stranger asked for must stay separate from the reads coverage waits on, or \
         the knot spends a whole pass on it without renewing anything"
    );
    assert!(
        matches!(index.keys().work(at(100), sweep()), Resolved::Ready(work) if work.suspected.is_empty()),
        "one miss is worth one reread, so a stranger offering keys can't set the fill's pace"
    );

    index.keys().note_miss();
    assert!(
        matches!(index.keys().work(at(159), sweep()), Resolved::Ready(work) if work.suspected.is_empty()),
        "a stranger can offer an unsigned key for free, so a knot that swept a moment ago \
         mustn't spend another read at every PDS in the grant set"
    );
    assert!(
        matches!(index.keys().work(at(160), sweep()), Resolved::Ready(work) if work.suspected.len() == 1),
        "the miss the floor delayed is honored once the floor has passed, so a key published \
         between passes is still picked up"
    );

    assert!(
        matches!(index.keys().work(at(1_799), sweep()), Resolved::Ready(work) if work.due.is_empty()),
        "an account inside the first half of its lease is left alone"
    );
    let Resolved::Ready(work) = index.keys().work(at(1_800), sweep()) else {
        panic!("the registry is folded, so the knot knows who may push");
    };
    assert_eq!(
        work.due.len(),
        1,
        "the fill renews at the halfway mark, so a pass no longer than the lease it is working \
         to finishes before anything expires"
    );
    assert!(
        work.complete,
        "the lease the account still has answers for it, so a knot busy renewing keeps \
         refusing keys the accounts on file don't publish instead of opening the handshake \
         to every stranger"
    );
}

#[test]
fn an_unfolded_repo_keeps_the_pass_partial_and_an_unopenable_repo_never_grants() {
    let world = World::new();
    let folded = repo_did("squid");
    let never_created = repo_did("limpet");
    world.layout.create(&folded).unwrap();
    let registry = world.seed_registry(&folded);
    world.register_owned(&never_created, "limpet", "cuttle", registry);
    let index = world.index();
    index.refresh_registry().unwrap();
    index.refresh_collaborators(&folded).unwrap();

    let Resolved::Ready(work) = index.keys().work(at(0), sweep()) else {
        panic!("one repo the knot hasn't folded mustn't stop it filling the keys it can enumerate");
    };
    assert_eq!(
        work.hosted,
        HostedCoverage::Partial { unread: 1 },
        "the pass must know it read less than the whole roll, or it evicts the accounts it \
         couldn't enumerate"
    );
    assert!(
        !work.due.is_empty(),
        "the owner of the repo that did fold still needs its keys read"
    );
    assert!(
        !work.complete,
        "a partial roll mustn't let the fill call the set complete, or the knot refuses a \
         pusher whose repo it never managed to read"
    );

    assert_eq!(
        index.warm_collaborators(),
        1,
        "the repo registered without ever being created is what the knot can't open"
    );
    let Resolved::Ready(work) = index.keys().work(at(0), sweep()) else {
        panic!("the registry is folded, so the knot knows who may push");
    };
    assert_eq!(
        work.hosted,
        HostedCoverage::Whole,
        "a repo the knot can't open won't serve a push, so counting it as unread would leave \
         the set warming and the handshake open to every stranger for good"
    );
    assert_eq!(
        work.pushers.as_slice(),
        [acc("nel")],
        "only the repo the knot can open grants anybody"
    );

    index
        .keys()
        .record(&acc("nel"), vec![OfferedKey::from_bytes(vec![7])], hour(0));
    assert!(
        pushers_complete(&index, at(0)),
        "with the one readable repo's owner on file the set is complete, so the knot can go \
         back to refusing keys the accounts on file don't publish"
    );
}

#[test]
fn an_acl_write_puts_the_key_set_back_to_warming_even_mid_pass() {
    let (world, index) = folded();
    index
        .keys()
        .record(&acc("nel"), vec![OfferedKey::from_bytes(vec![7])], hour(0));
    let Resolved::Ready(work) = index.keys().work(at(0), sweep()) else {
        panic!("the registry is folded, so the knot knows who may push");
    };
    assert!(work.complete, "the one pusher's keys are on file");
    index.keys().mark_ready(work.generation);
    assert_eq!(index.keys().coverage(), Coverage::Ready);

    let roll = world.seed_members();
    index.refresh_members().unwrap();
    assert_eq!(
        index.keys().coverage(),
        Coverage::Warming,
        "the knot mustn't check a grant it has never read keys for against a set it calls \
         complete, or a new collaborator is refused until the next fill pass"
    );

    let Resolved::Ready(rereading) = index.keys().work(at(0), sweep()) else {
        panic!("the registry is folded, so the knot knows who may push");
    };
    world.add_member(roll, "teq", 5);
    index.refresh_members().unwrap();
    index.keys().mark_ready(rereading.generation);
    assert_eq!(
        index.keys().coverage(),
        Coverage::Warming,
        "a pass claims the grant set it read, so a grant written while it was reading keys \
         mustn't count as covered, or whoever it grants is refused at the handshake"
    );
}

#[test]
fn an_account_the_budget_cant_fit_is_checked_against_its_pds_without_delaying_coverage() {
    let world = World::new();
    let folded = repo_did("squid");
    let second = repo_did("limpet");
    world.layout.create(&folded).unwrap();
    world.layout.create(&second).unwrap();
    let registry = world.seed_registry(&folded);
    world.register_owned(&second, "limpet", "cuttle", registry);
    let index = world.index_within(KeyBudget::from_bytes(560));
    index.refresh_registry().unwrap();
    index.warm_collaborators();
    let fat = |seed: u8| OfferedKey::from_bytes(vec![seed; 48]);

    assert_eq!(
        index.keys().record(&acc("nel"), vec![fat(1)], hour(0)),
        KeyRecord::Stored
    );
    assert_eq!(
        index.keys().record(&acc("cuttle"), vec![fat(2)], hour(0)),
        KeyRecord::Unheld
    );
    assert!(
        pushers_complete(&index, at(0)),
        "a full budget mustn't leave the set warming for good, or the knot spends the rest of \
         its life accepting every key offered to it"
    );
    let Resolved::Ready(later) = index.keys().work(at(3_601), sweep()) else {
        panic!("the registry is folded, so the knot knows who may push");
    };
    assert!(
        later.due.as_slice().contains(&acc("cuttle")),
        "the knot tries the account again once the lease runs out, so a budget that frees up \
         starts keeping its keys. Every pass until then costs one read rather than a reread \
         of the whole grant set"
    );
}

#[test]
fn a_key_read_outside_its_lease_stops_answering_for_its_publisher() {
    let world = World::new();
    let index = world.index();
    let key = OfferedKey::from_bytes(vec![7]);
    index.keys().record(&acc("nel"), vec![key.clone()], hour(0));

    assert_eq!(
        index.owner_of_key(&key, at(3_599)),
        Resolved::Ready(Some(acc("nel"))),
        "inside the lease the set answers for the account that published the key"
    );
    assert_eq!(
        index.keys().publisher_among(&[acc("nel")], &key, at(3_599)),
        Some(acc("nel"))
    );
    assert_eq!(
        index.owner_of_key(&key, at(3_601)),
        Resolved::Ready(None),
        "past the lease the knot no longer knows the account publishes the key, so a key \
         revoked while the fill was behind mustn't keep clearing the handshake"
    );
    assert_eq!(
        index.keys().publisher_among(&[acc("nel")], &key, at(3_601)),
        None,
        "a push check is bound by the same lease, or a key revoked while the fill was behind \
         keeps authorizing pushes"
    );
}

#[test]
fn a_warming_member_roll_still_yields_the_accounts_that_may_push() {
    let (_world, index) = folded();

    let Resolved::Ready(work) = index.keys().work(at(0), sweep()) else {
        panic!("the registry is folded, so the knot knows who may push");
    };
    assert_eq!(work.pushers.len(), 1, "the one hosted repo has one owner");
    assert_eq!(
        work.due.len(),
        1,
        "the owner of the one hosted repo may push and hasn't had its keys read yet"
    );
    assert!(
        work.members.is_warming(),
        "an unfolded member roll mustn't stop the knot reading the keys it checks pushes against"
    );
}

#[test]
fn a_reprieve_coasts_on_the_last_read_until_its_budget_runs_out() {
    let (_world, index) = folded();
    let key = OfferedKey::from_bytes(vec![7]);
    index.keys().record(&acc("nel"), vec![key.clone()], hour(0));

    assert_eq!(
        index
            .keys()
            .reprieve(&acc("nel"), at(10), grace(), hour(10)),
        KeyReprieved::Extended
    );
    assert!(
        index.keys().is_fresh(&acc("nel"), at(3_599)),
        "a reread the knot attempted early and couldn't finish mustn't cut the lease it already \
         has down to one retry, or a flaky PDS multiplies what the knot reads from it"
    );

    assert_eq!(
        index
            .keys()
            .reprieve(&acc("nel"), at(4_000), grace(), hour(4_000)),
        KeyReprieved::Extended
    );
    assert!(
        pushers_complete(&index, at(4_000)),
        "the reprieve keeps the keys the knot last read, so one unreachable PDS mustn't reopen \
         the knot to every offered key"
    );
    assert_eq!(
        index.owner_of_key(&key, at(4_000)),
        Resolved::Ready(Some(acc("nel")))
    );
    assert!(
        !index.keys().is_fresh(&acc("nel"), at(4_301)),
        "a reprieve is one retry's worth, so the knot tries the account again shortly"
    );

    assert_eq!(
        index
            .keys()
            .reprieve(&acc("nel"), at(21_500), grace(), hour(21_500)),
        KeyReprieved::Extended
    );
    assert!(
        !index.keys().is_fresh(&acc("nel"), at(21_600)),
        "the last reprieve before the budget runs out mustn't stretch past it"
    );

    assert_eq!(
        index
            .keys()
            .reprieve(&acc("nel"), at(21_601), grace(), hour(21_601)),
        KeyReprieved::Exhausted,
        "past the reprieve budget the knot stops coasting on keys it hasn't reread"
    );
    assert_eq!(
        index.owner_of_key(&key, at(21_601)),
        Resolved::Ready(None),
        "a key revoked while its PDS was unreachable mustn't keep authorizing pushes forever"
    );
}

#[test]
fn an_account_the_knot_never_reads_stops_delaying_the_whole_set() {
    let (_world, index) = folded();
    let offered = OfferedKey::from_bytes(vec![7]);

    assert_eq!(
        index.keys().reprieve(&acc("nel"), at(0), grace(), hour(0)),
        KeyReprieved::Pending,
        "an account the knot has never managed to read is still unread after a reprieve"
    );
    assert!(
        !pushers_complete(&index, at(0)),
        "an unread account mustn't count toward a complete set, or the knot refuses the keys \
         it has yet to fetch"
    );
    assert!(
        matches!(index.keys().work(at(299), sweep()), Resolved::Ready(work) if work.due.is_empty()),
        "a PDS that just refused the knot is left alone until the retry comes round"
    );
    assert!(
        matches!(index.keys().work(at(400), sweep()), Resolved::Ready(work) if work.due.len() == 1),
        "the knot retries an unread account every reprieve interval rather than every pass"
    );

    assert_eq!(
        index
            .keys()
            .reprieve(&acc("nel"), at(21_601), grace(), hour(21_601)),
        KeyReprieved::Exhausted,
        "the reprieve budget runs from the first failure, so a PDS that never answers stops \
         being a pending read"
    );
    assert!(
        pushers_complete(&index, at(21_601)),
        "one dead account mustn't keep the set warming forever, or the knot spends its whole \
         life accepting every key offered to it"
    );
    assert_eq!(
        index
            .keys()
            .publisher_among(&[acc("nel")], &offered, at(21_601)),
        None,
        "giving up on an account records what the knot knows, an empty key set"
    );
}

#[test]
fn concurrent_reads_of_one_account_never_leave_a_key_answering_for_a_set_it_left() {
    let world = World::new();
    let index = Arc::new(world.index());
    let hour = KeyTtl::from_secs(3_600).lease_from(at(0));
    let key = |i: u8| OfferedKey::from_bytes(vec![i]);

    (0..2_000u32).for_each(|round| {
        let first = key((round % 7) as u8);
        let second = key(7 + (round % 7) as u8);
        let start = std::sync::Barrier::new(6);
        std::thread::scope(|scope| {
            (0..6).for_each(|slot| {
                let index = Arc::clone(&index);
                let keys = match slot % 2 {
                    0 => vec![first.clone()],
                    _ => vec![second.clone()],
                };
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    index.keys().record(&acc("nel"), keys, hour)
                });
            });
        });

        [first, second].iter().for_each(|key| {
            let reverse = index.owner_of_key(key, at(0));
            let forward = index.keys().publisher_among(&[acc("nel")], key, at(0));
            assert_eq!(
                reverse,
                Resolved::Ready(forward),
                "a key the account no longer publishes must stop answering for it, or two reads \
                 arriving at once leave a revoked key authorizing pushes for good"
            );
        });
    });
}

fn folded() -> (World, knot_index::Index) {
    let world = World::new();
    let repo = repo_did("squid");
    world.layout.create(&repo).unwrap();
    world.seed_registry(&repo);
    let index = world.index();
    index.refresh_registry().unwrap();
    index.warm_collaborators();
    (world, index)
}

fn grace() -> KeyReprieve {
    KeyReprieve::from_secs(300, 21_600)
}

fn hour(from: i64) -> knot_index::KeyLease {
    KeyTtl::from_secs(3_600).lease_from(at(from))
}

fn sweep() -> SweepFloor {
    SweepFloor::from_secs(60)
}

fn pushers_complete(index: &knot_index::Index, now: knot_types::UnixSeconds) -> bool {
    matches!(index.keys().work(now, sweep()), Resolved::Ready(work) if work.complete)
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
