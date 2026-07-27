mod common;

use common::{
    account, at, build_members, cob_ref, fixture, forked_members, forked_members_object, grant,
    home, members_store, owner_of, registration, registry_with, rkey, signer, write_cob_commit,
};
use knot_cob::{ChangePayload, CobError, CobHome, CobId, CobStore};
use knot_cobs::{
    CollaboratorsChange, ImportError, MembersChange, MembersCob, RegistryChange, RegistryError,
    Removal, RepoRef, RepoRegistryCob, add_member, deregister_repo, register_repo, verify_cob_ref,
};
use knot_git::RefUpdate;
use knot_runtime::Signer;
use knot_types::{ActorId, OwnerDid, RepoDid, TypeName};
use serde::Serialize;

#[test]
fn forked_acl_is_rejected_not_merged() {
    let forked = forked_members(
        1,
        (MembersChange::Add(grant("seed", "seed", 1)), 1),
        (
            MembersChange::Remove(Removal {
                subject: account("nel"),
            }),
            2,
        ),
        (MembersChange::Add(grant("nel", "olaren", 3)), 3),
        (MembersChange::Add(grant("teq", "teq", 4)), 4),
    );
    assert!(
        matches!(forked, Err(CobError::ForkedHistory { .. })),
        "forked ACL is refused regardless of which branch a merge would favor"
    );
}

#[test]
fn linear_member_semantics() {
    let readd = build_members(
        2,
        &[
            (MembersChange::Add(grant("nel", "olaren", 1)), 1),
            (
                MembersChange::Remove(Removal {
                    subject: account("nel"),
                }),
                2,
            ),
            (MembersChange::Add(grant("nel", "teq", 3)), 3),
        ],
    );
    assert!(
        readd.contains(&account("nel")),
        "linear re-add after a remove is a legitimate decision and takes effect"
    );

    let stale_remove = build_members(
        20,
        &[
            (MembersChange::Add(grant("nel", "olaren", 5)), 5),
            (
                MembersChange::Remove(Removal {
                    subject: account("nel"),
                }),
                2,
            ),
        ],
    );
    assert!(
        !stale_remove.contains(&account("nel")),
        "in a linear chain Remove is Add's child, so it applies last even with an older timestamp"
    );

    let signed_by_one = build_members(1, &[(MembersChange::Add(grant("nel", "olaren", 9)), 1)]);
    assert_eq!(
        signed_by_one.get(&account("nel")).unwrap().added_by,
        account("olaren"),
        "added_by is whatever the payload claims, unrelated to who signed"
    );
    let signed_by_another =
        build_members(99, &[(MembersChange::Add(grant("nel", "olaren", 9)), 1)]);
    assert_eq!(
        signed_by_one, signed_by_another,
        "a different signing key over an identical payload yields identical state"
    );

    let once = build_members(
        10,
        &[
            (MembersChange::Add(grant("nel", "nel", 1)), 1),
            (
                MembersChange::Remove(Removal {
                    subject: account("nel"),
                }),
                2,
            ),
        ],
    );
    let twice = build_members(
        10,
        &[
            (MembersChange::Add(grant("nel", "nel", 1)), 1),
            (
                MembersChange::Remove(Removal {
                    subject: account("nel"),
                }),
                2,
            ),
            (
                MembersChange::Remove(Removal {
                    subject: account("nel"),
                }),
                3,
            ),
        ],
    );
    assert_eq!(once, twice, "replaying a remove is idempotent");
    assert!(once.is_empty());

    let created_at = build_members(
        13,
        &[
            (MembersChange::Add(grant("nel", "olaren", 100)), 1),
            (
                MembersChange::Remove(Removal {
                    subject: account("nel"),
                }),
                2,
            ),
            (MembersChange::Add(grant("nel", "teq", 50)), 3),
        ],
    );
    let entry = created_at.get(&account("nel")).unwrap();
    assert_eq!(entry.added_by, account("teq"), "last linear Add wins");
    assert_eq!(
        entry.created_at,
        at(50),
        "the later Add's created_at takes effect even though it is older than an earlier entry's"
    );
}

#[test]
fn verify_rejects_a_change_with_a_forged_signature() {
    let (_dir, repo) = fixture();
    let nsid = MembersChange::type_name();
    let owner = owner_of(32);
    let payload = MembersChange::Add(grant("nel", "nel", 1)).encode().unwrap();
    let root = write_cob_commit(&repo, &nsid, &payload, &[], &owner, 1);
    let object = CobId::new(root);
    repo.update_ref(&RefUpdate::Create {
        name: cob_ref(&nsid, object),
        new: root,
    })
    .unwrap();

    let store = CobStore::new(&repo);
    assert!(
        store.get::<MembersCob>(object).is_ok(),
        "read path materializes without checking signatures, by design"
    );
    assert!(
        matches!(
            store.verify::<MembersCob>(&home(), object, &owner),
            Err(CobError::UnverifiedChange { .. })
        ),
        "import verification catches forged signature the read path trusts"
    );
}

#[derive(Serialize)]
struct WireRegister<'a> {
    op: &'a str,
    data: WireRegistration<'a>,
}

#[derive(Serialize)]
struct WireRegistration<'a> {
    owner: &'a str,
    rkey: &'a str,
    name: &'a str,
    repo: &'a str,
    created_at: i64,
}

fn encode_register(rkey: &str, name: &str) -> Vec<u8> {
    serde_ipld_dagcbor::to_vec(&WireRegister {
        op: "register",
        data: WireRegistration {
            owner: "did:plc:nel",
            rkey,
            name,
            repo: "did:plc:squid",
            created_at: 1,
        },
    })
    .unwrap()
}

#[test]
fn malformed_repo_name_or_rkey_is_rejected_at_decode() {
    assert!(
        RegistryChange::decode(&encode_register("anemone", "anemone")).is_ok(),
        "control: well-formed wire payload decodes"
    );
    assert!(
        RegistryChange::decode(&encode_register("anemone", "../../etc/passwd")).is_err(),
        "traversal repo name fails newtype validation during decode, never reaching a ref"
    );
    assert!(
        RegistryChange::decode(&encode_register("anemone", "refs/heads/main")).is_err(),
        "name with path separators is rejected at decode"
    );
    assert!(
        RegistryChange::decode(&encode_register("not a record key", "anemone")).is_err(),
        "rkey outside record-key grammar is rejected at decode"
    );
    assert!(
        RegistryChange::decode(&encode_register("..", "anemone")).is_err(),
        "reserved '..' rkey is rejected at decode"
    );
}

#[test]
fn registry_handler_guards() {
    let (_dir, repo) = fixture();
    let key = signer(70);
    let store = CobStore::new(&repo);
    let object = registry_with(&repo, &key, "anemone", "squid");
    let nel = || OwnerDid::new("did:plc:nel").unwrap();
    let squid = || RepoDid::new("did:plc:squid").unwrap();

    let already = register_repo(
        &store,
        &home(),
        object,
        registration("olaren", "fork", "squid", 2),
        &key,
        at(2),
    );
    assert!(
        matches!(already, Err(RegistryError::AlreadyRegistered { .. })),
        "a repo DID already registered elsewhere cannot be claimed again"
    );

    let unregistered = deregister_repo(
        &store,
        &home(),
        object,
        RepoRef {
            owner: nel(),
            rkey: rkey("barnacle"),
        },
        squid(),
        &key,
        at(3),
    );
    assert!(matches!(
        unregistered,
        Err(RegistryError::NotRegistered { .. })
    ));

    let mismatch = deregister_repo(
        &store,
        &home(),
        object,
        RepoRef {
            owner: nel(),
            rkey: rkey("anemone"),
        },
        RepoDid::new("did:plc:whelk").unwrap(),
        &key,
        at(4),
    );
    assert!(
        matches!(mismatch, Err(RegistryError::RepoMismatch { .. })),
        "deregister whose expected repo doesn't match the keyed one is refused"
    );
    assert_eq!(
        store
            .get::<RepoRegistryCob>(object)
            .unwrap()
            .into_state()
            .resolve(&nel(), &rkey("anemone")),
        Some(&squid()),
        "a refused deregister left the registration intact"
    );

    register_repo(
        &store,
        &home(),
        object,
        registration("nel", "barnacle", "whelk", 5),
        &key,
        at(5),
    )
    .unwrap();
    assert_eq!(
        store
            .get::<RepoRegistryCob>(object)
            .unwrap()
            .into_state()
            .owner_of(&RepoDid::new("did:plc:whelk").unwrap()),
        Some(nel()),
        "a fresh repo DID lands"
    );

    deregister_repo(
        &store,
        &home(),
        object,
        RepoRef {
            owner: nel(),
            rkey: rkey("anemone"),
        },
        squid(),
        &key,
        at(6),
    )
    .unwrap();
    let after_deregister = store.get::<RepoRegistryCob>(object).unwrap().into_state();
    assert!(
        after_deregister.resolve(&nel(), &rkey("anemone")).is_none(),
        "a matching deregister removes the keyed repo"
    );
    assert_eq!(
        after_deregister.resolve(&nel(), &rkey("barnacle")),
        Some(&RepoDid::new("did:plc:whelk").unwrap()),
        "deregistering one repo leaves its sibling resolving"
    );
}

#[test]
fn add_member_handler_lands_a_grant() {
    let (_dir, repo) = fixture();
    let key = signer(81);
    let store = CobStore::new(&repo);
    let created = store
        .create(
            &home(),
            &MembersChange::Add(grant("nel", "nel", 1)),
            &key,
            at(1),
        )
        .unwrap();

    add_member(
        &store,
        &home(),
        created.object,
        grant("olaren", "nel", 2),
        &key,
        at(2),
    )
    .unwrap();

    let members = store
        .get::<MembersCob>(created.object)
        .unwrap()
        .into_state();
    assert!(members.contains(&account("olaren")));
}

#[test]
fn verify_cob_ref_boundary_cases() {
    let (_dir, repo, key, object) = members_store(
        90,
        &[
            (MembersChange::Add(grant("nel", "nel", 1)), 1),
            (MembersChange::Add(grant("olaren", "nel", 2)), 2),
        ],
    );
    let store = CobStore::new(&repo);
    let owner = ActorId::from_secp256k1(key.public_key().as_bytes());
    let refname = cob_ref(&MembersChange::type_name(), object);

    assert!(
        store.verify::<MembersCob>(&home(), object, &owner).is_ok(),
        "every change is validly signed by the owning key"
    );
    assert!(
        matches!(
            store.verify::<MembersCob>(&home(), object, &owner_of(31)),
            Err(CobError::UnverifiedChange { .. })
        ),
        "a change not authored by the claimed owner is refused at import"
    );

    assert_eq!(
        verify_cob_ref(&store, &home(), &refname, &owner).unwrap(),
        object,
        "a genuine object verifies through the namespace dispatcher"
    );
    assert!(matches!(
        verify_cob_ref(&store, &home(), &refname, &owner_of(91)),
        Err(ImportError::Cob(CobError::UnverifiedChange { .. }))
    ));

    let elsewhere = CobHome::from(&RepoDid::new("did:plc:limpet").unwrap());
    assert!(
        matches!(
            verify_cob_ref(&store, &elsewhere, &refname, &owner),
            Err(ImportError::Cob(CobError::UnverifiedChange { .. }))
        ),
        "an object pushed under a different repo home is refused at import"
    );

    assert!(matches!(
        verify_cob_ref(
            &store,
            &home(),
            &knot_types::RefName::new("refs/heads/main").unwrap(),
            &owner,
        ),
        Err(ImportError::NotCobRef(_))
    ));

    let stray = cob_ref(&TypeName::new("sh.tangled.test.unknown").unwrap(), object);
    assert!(matches!(
        verify_cob_ref(&store, &home(), &stray, &owner),
        Err(ImportError::UnknownType(_))
    ));

    let collaborators = store
        .create(
            &home(),
            &CollaboratorsChange::Add(grant("lyna", "nel", 1)),
            &key,
            at(1),
        )
        .unwrap()
        .object;
    let collab_ref = cob_ref(&CollaboratorsChange::type_name(), collaborators);
    assert_eq!(
        verify_cob_ref(&store, &home(), &collab_ref, &owner).unwrap(),
        collaborators,
        "the dispatcher routes a second namespace to its own resolver, not a hardcoded type"
    );

    let (_forked_dir, forked_repo, forked) = forked_members_object(
        95,
        (MembersChange::Add(grant("seed", "seed", 1)), 1),
        (MembersChange::Add(grant("nel", "olaren", 2)), 2),
        (
            MembersChange::Remove(Removal {
                subject: account("nel"),
            }),
            3,
        ),
        (MembersChange::Add(grant("teq", "teq", 4)), 4),
    );
    let forked_store = CobStore::new(&forked_repo);
    let forked_ref = cob_ref(&MembersChange::type_name(), forked);
    assert!(
        matches!(
            verify_cob_ref(&forked_store, &home(), &forked_ref, &owner_of(95)),
            Err(ImportError::Cob(CobError::ForkedHistory { .. }))
        ),
        "forked linear history is refused at import alongside the signature check"
    );
}
