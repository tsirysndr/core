use knot_bench::{RepoCount, build_registry};
use knot_index::Resolved;

#[test]
fn the_registry_fixture_boots_without_folding_collaborators() {
    let registry = build_registry(RepoCount::new(8));
    let index = registry.index();
    index.rebuild().unwrap();

    registry.dids().iter().for_each(|repo| {
        assert_eq!(
            index.is_collaborator(repo, &knot_types::AccountDid::new("did:plc:nel").unwrap()),
            Resolved::Warming,
            "rebuild must not fold any collaborator COB, so every repo reads warming until first access"
        );
    });

    let first = &registry.dids()[0];
    index.ensure_collaborators(first).unwrap();
    assert!(
        !index
            .is_collaborator(first, &knot_types::AccountDid::new("did:plc:nel").unwrap())
            .is_warming(),
        "repo folds on first access"
    );
}
