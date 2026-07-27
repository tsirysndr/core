use std::collections::BTreeSet;

use knot_git::Repo;
use knot_maintenance::{GeometricFactor, Options, PruneGrace, run_repo};
use knot_types::{ObjectFormat, Oid, RefName};

mod common;
use common::{
    EMPTY_TREE_SHA1, chain, create_repo, delete_ref, empty_tree, fsck_clean, git_available,
    has_cruft_pack, idx_stems, midx_verifies, now, options, reachable_objects, set_ref,
};

fn opts(factor: u64, grace: PruneGrace) -> Options {
    Options {
        geometric_factor: GeometricFactor::new(factor),
        prune_grace: grace,
        commit_graph: false,
        bitmap: false,
        ..options()
    }
}

fn main_ref(repo: &Repo) -> Option<Oid> {
    repo.find_ref(&RefName::new("refs/heads/main").unwrap())
        .unwrap()
}

fn no_reachable_loss(format: ObjectFormat) {
    if !git_available() {
        eprintln!("skipping geometric differential: git unavailable");
        return;
    }
    let scan = tempfile::tempdir().unwrap();
    let empty = empty_tree(format);
    let repo = create_repo(scan.path(), format, "did:plc:scallop");
    let opts = opts(2, PruneGrace::from_secs(86_400));

    (0..6).fold((None, BTreeSet::new()), |(tip, seen), round| {
        let next = chain(&repo, empty, round * 2..round * 2 + 2, tip);
        set_ref(&repo, "refs/heads/main", next);
        run_repo(&repo, now(), &opts).unwrap();

        let r = Repo::open(repo.git().git_dir()).unwrap();
        assert!(fsck_clean(&r), "{format:?} r{round}: not fsck-clean");
        if let Some((ok, stderr)) = midx_verifies(&r) {
            assert!(ok, "{format:?} r{round} midx: {stderr}");
        }
        assert_eq!(
            main_ref(&r),
            Some(next),
            "{format:?} r{round}: main lost its tip"
        );
        let present = reachable_objects(&r);
        assert!(
            seen.is_subset(&present),
            "{format:?} r{round}: a reachable object went missing"
        );
        (Some(next), present)
    });
}

#[test]
fn geometric_no_reachable_loss_sha1() {
    no_reachable_loss(ObjectFormat::SHA1);
}

#[test]
fn geometric_no_reachable_loss_sha256() {
    no_reachable_loss(ObjectFormat::SHA256);
}

#[test]
fn geometric_keeps_the_large_pack_while_rolling_up_then_crufting_small_packs() {
    if !git_available() {
        eprintln!("skipping geometric behavior test: git unavailable");
        return;
    }
    let scan = tempfile::tempdir().unwrap();
    let repo = create_repo(scan.path(), ObjectFormat::SHA1, "did:plc:conch");
    let opts = opts(2, PruneGrace::from_secs(86_400));

    let big = chain(&repo, EMPTY_TREE_SHA1, 0..10, None);
    set_ref(&repo, "refs/heads/main", big);
    run_repo(&repo, now(), &opts).unwrap();
    let large = idx_stems(&repo);
    assert_eq!(
        large.len(),
        1,
        "the initial repack settles to one large pack"
    );

    let advanced = chain(&repo, EMPTY_TREE_SHA1, 100..102, Some(big));
    set_ref(&repo, "refs/heads/main", advanced);
    run_repo(&repo, now(), &opts).unwrap();
    assert!(
        large.is_subset(&idx_stems(&repo)),
        "the large pack is kept verbatim"
    );
    assert_eq!(
        idx_stems(&repo).len(),
        2,
        "small additions roll into a second pack"
    );

    let feature = chain(&repo, EMPTY_TREE_SHA1, 200..202, Some(advanced));
    set_ref(&repo, "refs/heads/feature", feature);
    run_repo(&repo, now(), &opts).unwrap();
    delete_ref(&repo, "refs/heads/feature");

    let report = run_repo(&repo, now(), &opts).unwrap();
    assert!(
        report.prune.crufted.get() >= 1,
        "young rolled-up garbage is crufted"
    );
    assert!(
        large.is_subset(&idx_stems(&repo)) && has_cruft_pack(&repo),
        "large pack untouched, cruft written"
    );

    let reopened = Repo::open(repo.git().git_dir()).unwrap();
    assert!(
        fsck_clean(&reopened),
        "not fsck-clean after roll-up and cruft"
    );
    assert!(
        reopened.contains(advanced) && reopened.contains(feature),
        "reachable tip and young orphan both survive"
    );
}
