mod common;

use common::{account, at, did, fixture, grant, home, registration, rename, reopen, rkey, signer};
use knot_cob::CobStore;
use knot_cobs::{
    CollaboratorsChange, MembersChange, MembersCob, RegistryChange, RegistryError, Removal,
    RepoRegistryCob, register_repo, rename_repo,
};
use knot_types::{AccountDid, OwnerDid, RepoDid};

#[test]
fn members_roundtrip_and_reload_is_identical() {
    let (_dir, repo) = fixture();
    let key = signer(1);
    let store = CobStore::new(&repo);

    let created = store
        .create(
            &home(),
            &MembersChange::Add(grant("nel", "nel", 1)),
            &key,
            at(1),
        )
        .unwrap();
    store
        .update(
            &home(),
            created.object,
            &MembersChange::Add(grant("olaren", "nel", 2)),
            &key,
            at(2),
        )
        .unwrap();
    store
        .update(
            &home(),
            created.object,
            &MembersChange::Add(grant("teq", "nel", 3)),
            &key,
            at(3),
        )
        .unwrap();
    store
        .update(
            &home(),
            created.object,
            &MembersChange::Remove(Removal {
                subject: account("teq"),
            }),
            &key,
            at(4),
        )
        .unwrap();

    let object = store.get::<MembersCob>(created.object).unwrap();
    let state = object.state();
    assert!(state.contains(&account("nel")));
    assert_eq!(
        state.get(&account("olaren")).unwrap().added_by,
        account("nel")
    );
    assert!(!state.contains(&account("teq")));
    assert_eq!(state.len(), 2);

    let listed: Vec<&AccountDid> = state.entries().map(|(subject, _)| subject).collect();
    assert_eq!(listed, vec![&account("nel"), &account("olaren")]);

    let reopened = reopen(repo);
    let reloaded = CobStore::new(&reopened)
        .get::<MembersCob>(created.object)
        .unwrap();
    assert_eq!(state, reloaded.state());

    let (_dup_dir, dup_repo) = fixture();
    let dup_store = CobStore::new(&dup_repo);
    let dup = dup_store
        .create(
            &home(),
            &MembersChange::Add(grant("nel", "olaren", 7)),
            &key,
            at(1),
        )
        .unwrap();
    dup_store
        .update(
            &home(),
            dup.object,
            &MembersChange::Add(grant("nel", "olaren", 7)),
            &key,
            at(2),
        )
        .unwrap();
    let replayed = dup_store
        .get::<MembersCob>(dup.object)
        .unwrap()
        .into_state();
    assert_eq!(replayed.len(), 1, "replaying an identical add is a no-op");
    let entry = replayed.get(&account("nel")).unwrap();
    assert_eq!(entry.added_by, account("olaren"));
    assert_eq!(entry.created_at, at(7));

    let converge = |order: [(&str, &str, i64); 2]| {
        let (_dir, repo) = fixture();
        let store = CobStore::new(&repo);
        let [first, second] = order;
        let created = store
            .create(
                &home(),
                &MembersChange::Add(grant(first.0, first.1, first.2)),
                &key,
                at(1),
            )
            .unwrap();
        store
            .update(
                &home(),
                created.object,
                &MembersChange::Add(grant(second.0, second.1, second.2)),
                &key,
                at(2),
            )
            .unwrap();
        store
            .get::<MembersCob>(created.object)
            .unwrap()
            .into_state()
    };
    assert_eq!(
        converge([("nel", "nel", 10), ("olaren", "nel", 20)]),
        converge([("olaren", "nel", 20), ("nel", "nel", 10)]),
        "independent grants converge across write order"
    );
}

#[test]
fn collaborators_live_in_a_per_repo_cob_namespace() {
    let (_dir, repo) = fixture();
    let key = signer(2);
    let store = CobStore::new(&repo);
    store
        .create(
            &home(),
            &CollaboratorsChange::Add(grant("lyna", "nel", 1)),
            &key,
            at(1),
        )
        .unwrap();

    let cob_ref = repo.references().unwrap().into_iter().find(|record| {
        record
            .name
            .as_str()
            .contains("sh.tangled.repo.collaborator")
    });
    assert!(cob_ref.is_some(), "collaborators live under refs/cobs");
    assert!(repo.advertised_refs().unwrap().is_empty());
}

#[test]
fn registry_handler_semantics() {
    let (_dir, repo) = fixture();
    let key = signer(21);
    let store = CobStore::new(&repo);
    let nel = did::<OwnerDid>("nel");

    let created = store
        .create(
            &home(),
            &RegistryChange::Register(registration("nel", "anemone", "squid", 1)),
            &key,
            at(1),
        )
        .unwrap();
    let object = created.object;

    let clash = register_repo(
        &store,
        &home(),
        object,
        registration("nel", "anemone", "whelk", 2),
        &key,
        at(2),
    );
    assert!(
        matches!(clash, Err(RegistryError::RkeyTaken { .. })),
        "register cannot claim the canonical rkey of a live repo under the same owner"
    );
    assert_eq!(
        register_repo(
            &store,
            &home(),
            object,
            registration("nel", "anemone", "squid", 2),
            &key,
            at(2),
        )
        .unwrap(),
        None,
        "re-registering identical owner, rkey, and repo appends nothing"
    );

    let renamed = rename_repo(
        &store,
        &home(),
        object,
        rename("nel", "barnacle", "squid"),
        &key,
        at(2),
    )
    .unwrap();
    assert!(renamed.is_some(), "real rename appends a change");
    let state = store.get::<RepoRegistryCob>(object).unwrap().into_state();
    assert_eq!(
        state.resolve(&nel, &rkey("anemone")),
        Some(&did::<RepoDid>("squid")),
        "prior rkey keeps resolving as an alias"
    );
    assert_eq!(
        state.resolve(&nel, &rkey("barnacle")),
        Some(&did::<RepoDid>("squid"))
    );
    assert_eq!(
        state.record_of(&did("squid")).unwrap().rkey,
        rkey("barnacle")
    );

    let redundant = register_repo(
        &store,
        &home(),
        object,
        registration("nel", "barnacle", "squid", 3),
        &key,
        at(3),
    )
    .unwrap();
    assert_eq!(
        redundant, None,
        "re-register matching canonical owner and rkey appends nothing"
    );
    assert_eq!(
        store
            .get::<RepoRegistryCob>(object)
            .unwrap()
            .into_state()
            .resolve(&nel, &rkey("anemone")),
        Some(&did::<RepoDid>("squid")),
        "retained alias survives a redundant re-register"
    );

    store
        .update(
            &home(),
            object,
            &RegistryChange::Register(registration("nel", "kelp", "whelk", 4)),
            &key,
            at(4),
        )
        .unwrap();

    let taken = rename_repo(
        &store,
        &home(),
        object,
        rename("nel", "kelp", "squid"),
        &key,
        at(5),
    );
    assert!(
        matches!(taken, Err(RegistryError::RkeyTaken { .. })),
        "rename cannot take canonical rkey of another live repo"
    );

    let unhosted = rename_repo(
        &store,
        &home(),
        object,
        rename("nel", "limpet", "conch"),
        &key,
        at(6),
    );
    assert!(
        matches!(unhosted, Err(RegistryError::NotHosted { .. })),
        "rename of a repo with no registration is refused"
    );

    let moved = rename_repo(
        &store,
        &home(),
        object,
        rename("olaren", "uni", "squid"),
        &key,
        at(7),
    );
    assert!(
        matches!(moved, Err(RegistryError::OwnerMoved { .. })),
        "rename under an owner the repo no longer belongs to is refused"
    );

    assert_eq!(
        store
            .get::<RepoRegistryCob>(object)
            .unwrap()
            .into_state()
            .record_of(&did("squid"))
            .unwrap()
            .rkey,
        rkey("barnacle"),
        "refused renames left the canonical rkey untouched"
    );
}
