use knot_git::Repo;
use knot_maintenance::{Options, PruneGrace, ReflogRetention, run_repo};
use knot_types::{ObjectFormat, UnixSeconds};

mod common;
use common::{
    EMPTY_TREE_SHA1, assert_fsck_clean, commit_on, create_repo, delete_ref, empty_tree,
    has_cruft_pack, now, options, set_ref, set_reflog_seconds,
};

fn opts(grace: PruneGrace) -> Options {
    Options {
        prune_grace: grace,
        commit_graph: false,
        multi_pack_index: false,
        bitmap: false,
        ..options()
    }
}

fn retain_repeat_then_expire(format: ObjectFormat) {
    let scan = tempfile::tempdir().unwrap();
    let empty = empty_tree(format);
    let repo = create_repo(scan.path(), format, "did:plc:cuttle");

    let base = commit_on(&repo, empty, 0, Vec::new());
    let doomed = commit_on(&repo, empty, 1, vec![base]);
    set_ref(&repo, "refs/heads/main", base);
    set_ref(&repo, "refs/heads/feature", doomed);
    run_repo(&repo, now(), &opts(PruneGrace::from_secs(86_400))).unwrap();
    assert!(repo.contains(doomed), "doomed is packed while reachable");

    delete_ref(&repo, "refs/heads/feature");
    set_ref(
        &repo,
        "refs/heads/main",
        commit_on(&repo, empty, 2, vec![base]),
    );

    let retained = run_repo(&repo, now(), &opts(PruneGrace::from_secs(86_400))).unwrap();
    assert!(
        retained.prune.crufted.get() >= 1,
        "young unreachable objects are crufted, not dropped"
    );
    let reopened = Repo::open(repo.git().git_dir()).unwrap();
    assert!(
        has_cruft_pack(&reopened) && reopened.contains(doomed),
        "a cruft pack holds the young object"
    );
    assert_fsck_clean(&reopened);

    run_repo(&reopened, now(), &opts(PruneGrace::from_secs(86_400))).unwrap();
    let recycled = Repo::open(repo.git().git_dir()).unwrap();
    assert!(
        recycled.contains(doomed),
        "the young object survives a repeat cruft cycle within grace"
    );
    assert_fsck_clean(&recycled);

    assert!(
        run_repo(&recycled, now(), &opts(PruneGrace::from_secs(0)))
            .unwrap()
            .prune
            .ran
    );
    let settled = Repo::open(repo.git().git_dir()).unwrap();
    assert!(
        !settled.contains(doomed),
        "the past-grace object is finally dropped"
    );
    assert!(
        settled.contains(base) && !has_cruft_pack(&settled),
        "base survives, no cruft lingers"
    );
    assert_fsck_clean(&settled);
}

#[test]
fn cruft_retains_survives_a_repeat_cycle_then_expires_sha1() {
    retain_repeat_then_expire(ObjectFormat::SHA1);
}

#[test]
fn cruft_retains_survives_a_repeat_cycle_then_expires_sha256() {
    retain_repeat_then_expire(ObjectFormat::SHA256);
}

#[test]
fn a_reflog_floor_above_the_retention_minimum_keeps_referenced_objects() {
    let scan = tempfile::tempdir().unwrap();
    let repo = create_repo(scan.path(), ObjectFormat::SHA1, "did:plc:whelk");

    let base = commit_on(&repo, EMPTY_TREE_SHA1, 0, Vec::new());
    let doomed = commit_on(&repo, EMPTY_TREE_SHA1, 1, vec![base]);
    set_ref(&repo, "refs/heads/main", doomed);
    set_ref(
        &repo,
        "refs/heads/main",
        commit_on(&repo, EMPTY_TREE_SHA1, 2, vec![base]),
    );

    let opts = Options {
        reflog_floor: ReflogRetention::from_secs(3_000_000_000),
        ..opts(PruneGrace::from_secs(0))
    };
    run_repo(&repo, UnixSeconds::new(4_000_000_000), &opts).unwrap();

    let reopened = Repo::open(repo.git().git_dir()).unwrap();
    assert!(
        reopened.contains(doomed),
        "an object held only by a retained reflog entry is a prune root"
    );
}

#[test]
fn a_rewound_away_commit_survives_on_its_reflog_old_pointer_alone() {
    let scan = tempfile::tempdir().unwrap();
    let repo = create_repo(scan.path(), ObjectFormat::SHA1, "did:plc:periwinkle");

    let lost = commit_on(&repo, EMPTY_TREE_SHA1, 7, Vec::new());
    set_ref(&repo, "refs/heads/main", lost);
    let survivor = commit_on(&repo, EMPTY_TREE_SHA1, 9, Vec::new());
    set_ref(&repo, "refs/heads/main", survivor);

    set_reflog_seconds(&repo, "refs/heads/main", &[1_000_000, 2_000_000_000]);

    let opts = Options {
        reflog_floor: ReflogRetention::from_secs(500_000_000),
        ..opts(PruneGrace::from_secs(0))
    };
    run_repo(&repo, UnixSeconds::new(2_000_000_000), &opts).unwrap();

    let reopened = Repo::open(repo.git().git_dir()).unwrap();
    assert!(
        reopened.contains(lost),
        "a force-rewound commit reachable only through a reflog old-pointer must not be pruned"
    );
    assert!(
        reopened.contains(survivor),
        "the live tip survives the prune"
    );
    assert_fsck_clean(&reopened);
}
