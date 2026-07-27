use knot_cob::{CobHome, CobStore};
use knot_cobs::{CollaboratorsChange, MembersChange, Registration, RegistryChange};
use knot_git::Layout;
use knot_index::{Index, Resolved};
use knot_runtime::{K256Signer, SeededEntropy};
use knot_types::{KnotId, RepoDid, RepoName};

mod common;
use common::{acc, at, grant, own, rkey};

#[test]
fn the_meta_repo_round_trips_every_projection_from_git() {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::new(dir.path());
    let knot = KnotId::new("did:web:oyster.cafe").unwrap();
    let signer = K256Signer::generate(&SeededEntropy::new(1));

    let meta = layout.bootstrap_meta(&knot).unwrap();
    let store = CobStore::new(&meta);
    let members = store
        .create(
            &CobHome::from(&knot),
            &MembersChange::Add(grant("nel", "nel", 1)),
            &signer,
            at(1),
        )
        .unwrap();
    store
        .update(
            &CobHome::from(&knot),
            members.object,
            &MembersChange::Add(grant("olaren", "nel", 2)),
            &signer,
            at(2),
        )
        .unwrap();

    let repo = RepoDid::new("did:plc:squid").unwrap();
    store
        .create(
            &CobHome::from(&knot),
            &RegistryChange::Register(Registration {
                owner: own("nel"),
                rkey: rkey("anemone"),
                name: RepoName::new("anemone").unwrap(),
                repo: repo.clone(),
                created_at: at(1),
            }),
            &signer,
            at(1),
        )
        .unwrap();

    let git = layout.create(&repo).unwrap();
    CobStore::new(&git)
        .create(
            &CobHome::from(&repo),
            &CollaboratorsChange::Add(grant("lyna", "nel", 1)),
            &signer,
            at(1),
        )
        .unwrap();

    let meta_path = layout.meta_path(&knot).unwrap();
    let boot = || {
        let index = Index::new(meta_path.clone(), layout.clone());
        index.rebuild().unwrap();
        index.warm_collaborators();
        index
    };

    let first = boot();
    assert_eq!(first.is_member(&acc("nel")), Resolved::Ready(true));
    assert_eq!(first.is_member(&acc("olaren")), Resolved::Ready(true));
    assert_eq!(
        first.resolve_repo(&own("nel"), &rkey("anemone")),
        Resolved::Ready(Some(repo.clone()))
    );
    assert_eq!(
        first.is_collaborator(&repo, &acc("lyna")),
        Resolved::Ready(true)
    );

    let second = boot();
    ["nel", "olaren", "lyna", "stranger"]
        .into_iter()
        .for_each(|who| {
            assert_eq!(first.is_member(&acc(who)), second.is_member(&acc(who)));
            assert_eq!(
                first.is_collaborator(&repo, &acc(who)),
                second.is_collaborator(&repo, &acc(who)),
            );
        });
    assert_eq!(
        first.resolve_repo(&own("nel"), &rkey("anemone")),
        second.resolve_repo(&own("nel"), &rkey("anemone")),
    );
}
