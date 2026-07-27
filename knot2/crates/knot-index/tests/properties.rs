use knot_cob::{CobHome, CobStore};
use knot_cobs::{
    CollaboratorsChange, CollaboratorsCob, Grant, MembersChange, MembersCob, Registration,
    RegistryChange, Removal, Rename, RepoRef, RepoRegistryCob,
};
use knot_git::Repo;
use knot_index::{Index, Resolved};
use knot_types::{AccountDid, OwnerDid, RepoDid, RepoName, RepoRkey, UnixSeconds};
use proptest::prelude::*;

mod common;
use common::{World, meta_home};

fn acc(n: u8) -> AccountDid {
    AccountDid::new(format!("did:plc:s{n}")).unwrap()
}

fn owner(n: u8) -> OwnerDid {
    match n {
        0 => OwnerDid::new("did:plc:nel").unwrap(),
        _ => OwnerDid::new("did:plc:olaren").unwrap(),
    }
}

fn repo_rkey(n: u8) -> RepoRkey {
    RepoRkey::new(format!("r{n}")).unwrap()
}

fn repo_did(n: u8) -> RepoDid {
    RepoDid::new(format!("did:plc:r{n}")).unwrap()
}

fn grant(subject: u8, t: i64) -> Grant {
    Grant {
        subject: acc(subject),
        added_by: AccountDid::new("did:plc:nel").unwrap(),
        created_at: UnixSeconds::new(t),
    }
}

fn member_change(op: u8, subject: u8, t: i64) -> MembersChange {
    match op {
        0 => MembersChange::Add(grant(subject, t)),
        _ => MembersChange::Remove(Removal {
            subject: acc(subject),
        }),
    }
}

fn collaborator_change(op: u8, subject: u8, t: i64) -> CollaboratorsChange {
    match op {
        0 => CollaboratorsChange::Add(grant(subject, t)),
        _ => CollaboratorsChange::Remove(Removal {
            subject: acc(subject),
        }),
    }
}

fn registry_change(op: u8, who: u8, rkey: u8, repo: u8, t: i64) -> RegistryChange {
    match op {
        0 => RegistryChange::Register(Registration {
            owner: owner(who),
            rkey: repo_rkey(rkey),
            name: RepoName::new(format!("r{rkey}")).unwrap(),
            repo: repo_did(repo),
            created_at: UnixSeconds::new(t),
        }),
        1 => RegistryChange::Rename(Rename {
            owner: owner(who),
            rkey: repo_rkey(rkey),
            name: RepoName::new(format!("r{rkey}")).unwrap(),
            repo: repo_did(repo),
        }),
        _ => RegistryChange::Deregister(RepoRef {
            owner: owner(who),
            rkey: repo_rkey(rkey),
        }),
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 40, ..ProptestConfig::default() })]

    #[test]
    fn members_fold_equals_canonical_evaluate(
        ops in prop::collection::vec((0u8..2, 0u8..4), 1..14)
    ) {
        let world = World::seeded(7);
        let meta = Repo::open(&world.meta_path).unwrap();
        let store = CobStore::new(&meta);

        let incremental = Index::new(&world.meta_path, world.layout.clone());

        let (op0, subject0) = ops[0];
        let object = store
            .create(&meta_home(), &member_change(op0, subject0, 1), &world.signer, UnixSeconds::new(1))
            .unwrap()
            .object;
        incremental.refresh_members().unwrap();

        ops.iter().enumerate().skip(1).for_each(|(index, (op, subject))| {
            let t = index as i64 + 1;
            store
                .update(&meta_home(), object, &member_change(*op, *subject, t), &world.signer, UnixSeconds::new(t))
                .unwrap();
            incremental.refresh_members().unwrap();
        });

        let full = Index::new(&world.meta_path, world.layout.clone());
        full.rebuild().unwrap();

        let canonical = store.get::<MembersCob>(object).unwrap();
        let roster = canonical.state();
        let expected: Vec<Resolved<bool>> = (0u8..4)
            .map(|subject| Resolved::Ready(roster.contains(&acc(subject))))
            .collect();
        prop_assert_eq!(
            (0u8..4).map(|s| incremental.is_member(&acc(s))).collect::<Vec<_>>(),
            expected.clone()
        );
        prop_assert_eq!(
            (0u8..4).map(|s| full.is_member(&acc(s))).collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn collaborators_fold_equals_canonical_evaluate(
        ops in prop::collection::vec((0u8..2, 0u8..4), 1..14)
    ) {
        let world = World::seeded(8);
        let repo = repo_did(0);
        let git = world.layout.create(&repo).unwrap();
        let store = CobStore::new(&git);

        let incremental = Index::new(&world.meta_path, world.layout.clone());

        let (op0, subject0) = ops[0];
        let object = store
            .create(&CobHome::from(&repo), &collaborator_change(op0, subject0, 1), &world.signer, UnixSeconds::new(1))
            .unwrap()
            .object;
        incremental.rebuild().unwrap();
        incremental.refresh_collaborators(&repo).unwrap();

        ops.iter().enumerate().skip(1).for_each(|(index, (op, subject))| {
            let t = index as i64 + 1;
            store
                .update(&CobHome::from(&repo), object, &collaborator_change(*op, *subject, t), &world.signer, UnixSeconds::new(t))
                .unwrap();
            incremental.refresh_collaborators(&repo).unwrap();
        });

        let full = Index::new(&world.meta_path, world.layout.clone());
        full.rebuild().unwrap();
        full.refresh_collaborators(&repo).unwrap();

        let canonical = store.get::<CollaboratorsCob>(object).unwrap();
        let roster = canonical.state();
        let expected: Vec<Resolved<bool>> = (0u8..4)
            .map(|subject| Resolved::Ready(roster.contains(&acc(subject))))
            .collect();
        prop_assert_eq!(
            (0u8..4).map(|s| incremental.is_collaborator(&repo, &acc(s))).collect::<Vec<_>>(),
            expected.clone()
        );
        prop_assert_eq!(
            (0u8..4).map(|s| full.is_collaborator(&repo, &acc(s))).collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn registry_fold_equals_canonical_evaluate(
        ops in prop::collection::vec((0u8..3, 0u8..2, 0u8..4, 0u8..4), 1..14)
    ) {
        let world = World::seeded(9);
        let meta = Repo::open(&world.meta_path).unwrap();
        let store = CobStore::new(&meta);

        let incremental = Index::new(&world.meta_path, world.layout.clone());

        let (op0, who0, name0, repo0) = ops[0];
        let object = store
            .create(&meta_home(), &registry_change(op0, who0, name0, repo0, 1), &world.signer, UnixSeconds::new(1))
            .unwrap()
            .object;
        incremental.refresh_registry().unwrap();

        ops.iter().enumerate().skip(1).for_each(|(index, (op, who, name, repo))| {
            let t = index as i64 + 1;
            store
                .update(&meta_home(), object, &registry_change(*op, *who, *name, *repo, t), &world.signer, UnixSeconds::new(t))
                .unwrap();
            incremental.refresh_registry().unwrap();
        });

        let full = Index::new(&world.meta_path, world.layout.clone());
        full.rebuild().unwrap();

        let canonical = store.get::<RepoRegistryCob>(object).unwrap();
        let registry = canonical.state();
        let lookups: Vec<(u8, u8)> = (0u8..2)
            .flat_map(|who| (0u8..4).map(move |rkey| (who, rkey)))
            .collect();
        let expected: Vec<Resolved<Option<RepoDid>>> = lookups
            .iter()
            .map(|(who, rkey)| {
                Resolved::Ready(registry.resolve(&owner(*who), &repo_rkey(*rkey)).cloned())
            })
            .collect();
        prop_assert_eq!(
            lookups
                .iter()
                .map(|(who, rkey)| incremental.resolve_repo(&owner(*who), &repo_rkey(*rkey)))
                .collect::<Vec<_>>(),
            expected.clone()
        );
        prop_assert_eq!(
            lookups
                .iter()
                .map(|(who, rkey)| full.resolve_repo(&owner(*who), &repo_rkey(*rkey)))
                .collect::<Vec<_>>(),
            expected
        );
        let expected_records: Vec<_> = (0u8..4)
            .map(|n| {
                let record = registry.record_of(&repo_did(n));
                (
                    Resolved::Ready(record.map(|record| record.owner.clone())),
                    Resolved::Ready(record.map(|record| record.rkey.clone())),
                )
            })
            .collect();
        prop_assert_eq!(
            (0u8..4)
                .map(|n| (incremental.owner_of(&repo_did(n)), incremental.rkey_of(&repo_did(n))))
                .collect::<Vec<_>>(),
            expected_records.clone()
        );
        prop_assert_eq!(
            (0u8..4)
                .map(|n| (full.owner_of(&repo_did(n)), full.rkey_of(&repo_did(n))))
                .collect::<Vec<_>>(),
            expected_records
        );
    }

    #[test]
    fn two_rebuilds_are_observably_identical(
        members in prop::collection::vec((0u8..2, 0u8..6), 0..16),
        registry in prop::collection::vec((0u8..3, 0u8..2, 0u8..4, 0u8..4), 0..10),
    ) {
        let world = World::seeded(10);
        let meta = Repo::open(&world.meta_path).unwrap();
        let store = CobStore::new(&meta);

        if let Some(((op, subject), rest)) = members.split_first() {
            let object = store
                .create(&meta_home(), &member_change(*op, *subject, 1), &world.signer, UnixSeconds::new(1))
                .unwrap()
                .object;
            rest.iter().enumerate().for_each(|(index, (op, subject))| {
                let t = index as i64 + 2;
                store
                    .update(&meta_home(), object, &member_change(*op, *subject, t), &world.signer, UnixSeconds::new(t))
                    .unwrap();
            });
        }

        if let Some(((op, who, name, repo), rest)) = registry.split_first() {
            let object = store
                .create(&meta_home(), &registry_change(*op, *who, *name, *repo, 1), &world.signer, UnixSeconds::new(1))
                .unwrap()
                .object;
            rest.iter().enumerate().for_each(|(index, (op, who, name, repo))| {
                let t = index as i64 + 2;
                store
                    .update(&meta_home(), object, &registry_change(*op, *who, *name, *repo, t), &world.signer, UnixSeconds::new(t))
                    .unwrap();
            });
        }

        let first = Index::new(&world.meta_path, world.layout.clone());
        first.rebuild().unwrap();
        let second = Index::new(&world.meta_path, world.layout.clone());
        second.rebuild().unwrap();

        prop_assert_eq!(
            (0u8..6).map(|s| first.is_member(&acc(s))).collect::<Vec<_>>(),
            (0u8..6).map(|s| second.is_member(&acc(s))).collect::<Vec<_>>()
        );
        let lookups: Vec<(u8, u8)> = (0u8..2)
            .flat_map(|who| (0u8..4).map(move |rkey| (who, rkey)))
            .collect();
        prop_assert_eq!(
            lookups
                .iter()
                .map(|(who, rkey)| first.resolve_repo(&owner(*who), &repo_rkey(*rkey)))
                .collect::<Vec<_>>(),
            lookups
                .iter()
                .map(|(who, rkey)| second.resolve_repo(&owner(*who), &repo_rkey(*rkey)))
                .collect::<Vec<_>>()
        );
    }
}
