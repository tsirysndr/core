use std::collections::HashSet;

use knot_git::Repo;
use knot_maintenance::{Options, PruneGrace, RepackStatus, run_repo};
use knot_types::{ObjectFormat, Oid};

mod common;
use common::{
    commit, create_repo, delete_ref, git, has_bitmap, has_midx_bitmap, now, options, set_ref,
};

fn create(scan: &std::path::Path, did: &str) -> Repo {
    create_repo(scan, ObjectFormat::SHA1, did)
}

fn loose_count(repo: &Repo) -> usize {
    walkdir::WalkDir::new(repo.git().git_dir().join("objects"))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.len() == 2)
        })
        .count()
}

#[test]
fn a_single_pack_maintenance_pass_packs_graphs_bitmaps_prunes_then_settles() {
    let scan = tempfile::tempdir().unwrap();
    let repo = create(scan.path(), "did:plc:squid");

    let base = commit(&repo, 0, Vec::new());
    let main_tip = commit(&repo, 1, vec![base]);
    set_ref(&repo, "refs/heads/main", main_tip);
    set_ref(&repo, "refs/heads/feature", commit(&repo, 2, vec![base]));
    let orphan = commit(&repo, 9, vec![main_tip]);

    let info = repo.git().git_dir().join("objects/info");
    std::fs::create_dir_all(&info).unwrap();
    let leaked = info.join("commit-graph.knot-tmp.999999");
    std::fs::write(&leaked, b"a graph write that a kill -9 interrupted").unwrap();
    let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7 * 3600);
    std::fs::File::options()
        .write(true)
        .open(&leaked)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(long_ago))
        .unwrap();
    assert!(loose_count(&repo) >= 4 && repo.contains(orphan));

    let report = run_repo(&repo, now(), &options()).unwrap();
    assert_eq!(report.repack.status, RepackStatus::Repacked);
    assert!(
        report.repack.removed_loose.get() >= 4,
        "loose objects folded into the pack"
    );
    assert!(report.commit_graph && info.join("commit-graph").exists());
    assert!(
        !leaked.exists(),
        "maintenance sweeps a crashed graph write's temp once it is too old to still have a writer"
    );
    assert!(report.bitmap && has_bitmap(&repo));
    let (ok, stderr) = git(&repo, &["rev-list", "--test-bitmap", "main"]);
    assert!(ok, "canonical git accepts our bitmap: {stderr}");
    assert!(report.packed_refs.packed >= 1);
    assert!(
        report.prune.ran && report.prune.removed.get() >= 1,
        "the orphan is pruned"
    );

    let reopened = Repo::open(repo.git().git_dir()).unwrap();
    assert!(!reopened.contains(orphan) && reopened.contains(main_tip));
    let (clean, stderr) = git(&reopened, &["fsck", "--no-progress"]);
    assert!(clean, "fsck-clean after the pass: {stderr}");

    let settled = run_repo(&reopened, now(), &options()).unwrap();
    assert_eq!(settled.repack.status, RepackStatus::Clean);
    assert!(
        !settled.prune.ran && !settled.commit_graph && !settled.bitmap,
        "a settled repo is a no-op"
    );
    assert_eq!(settled.packed_refs.packed, 0);
}

#[test]
fn the_written_graph_verifies_and_accelerates_selection_through_an_octopus_merge() {
    let scan = tempfile::tempdir().unwrap();
    let repo = create(scan.path(), "did:plc:cuttle");

    let a = commit(&repo, 1, Vec::new());
    let b = commit(&repo, 2, Vec::new());
    let c = commit(&repo, 3, Vec::new());
    let pair = commit(&repo, 4, vec![a, b]);
    let octopus = commit(&repo, 5, vec![a, b, c]);
    let tip = commit(&repo, 6, vec![pair, octopus]);
    set_ref(&repo, "refs/heads/main", tip);

    let graph = repo.git().git_dir().join("objects/info/commit-graph");
    let closure = |r: &Repo| -> HashSet<Oid> {
        r.select_pack_objects(knot_git::Wants::new(&[tip]), knot_git::Haves::new(&[]))
            .unwrap()
            .into_iter()
            .collect()
    };
    let decode_closure = closure(&repo);
    assert!(!graph.exists());

    assert!(run_repo(&repo, now(), &options()).unwrap().commit_graph && graph.exists());
    let (ok, stderr) = git(&repo, &["commit-graph", "verify"]);
    assert!(
        ok,
        "git accepts the hand-written graph with an octopus: {stderr}"
    );

    let reopened = Repo::open(repo.git().git_dir()).unwrap();
    assert_eq!(
        closure(&reopened),
        decode_closure,
        "graph-accelerated selection equals the decode walk"
    );
}

#[test]
fn commit_graph_knob_lifecycle_skips_backfills_and_sweeps() {
    let scan = tempfile::tempdir().unwrap();
    let repo = create(scan.path(), "did:plc:limpet");

    let base = commit(&repo, 0, Vec::new());
    set_ref(&repo, "refs/heads/main", commit(&repo, 1, vec![base]));

    let off = Options {
        commit_graph: false,
        ..options()
    };
    let graph = repo.git().git_dir().join("objects/info/commit-graph");

    let first = run_repo(&repo, now(), &off).unwrap();
    assert_eq!(first.repack.status, RepackStatus::Repacked);
    assert!(
        !first.commit_graph && !graph.exists(),
        "no graph is written when the knob is off"
    );

    let reopened = Repo::open(repo.git().git_dir()).unwrap();
    let backfill = run_repo(&reopened, now(), &options()).unwrap();
    assert!(
        backfill.commit_graph && graph.exists(),
        "a packed repo with no graph backfills it"
    );
    assert_eq!(backfill.repack.status, RepackStatus::Clean);

    let settled = Repo::open(repo.git().git_dir()).unwrap();
    assert!(
        !run_repo(&settled, now(), &options()).unwrap().commit_graph,
        "a present graph settles to no-op"
    );

    let swept = Repo::open(repo.git().git_dir()).unwrap();
    assert!(!run_repo(&swept, now(), &off).unwrap().commit_graph);
    assert!(
        !graph.exists(),
        "flipping the knob off sweeps the orphan graph"
    );
}

#[test]
fn cruft_second_pack_gets_a_midx_bitmap_canonical_git_accepts() {
    let scan = tempfile::tempdir().unwrap();
    let repo = create(scan.path(), "did:plc:mussel");

    let base = commit(&repo, 0, Vec::new());
    let doomed = commit(&repo, 1, vec![base]);
    set_ref(&repo, "refs/heads/main", base);
    set_ref(&repo, "refs/heads/feature", doomed);

    let opts = Options {
        prune_grace: PruneGrace::from_secs(86_400),
        ..options()
    };
    run_repo(&repo, now(), &opts).unwrap();

    delete_ref(&repo, "refs/heads/feature");
    set_ref(&repo, "refs/heads/main", commit(&repo, 2, vec![base]));

    let report = run_repo(&repo, now(), &opts).unwrap();
    assert!(
        report.prune.crufted.get() >= 1,
        "young unreachable object is crufted"
    );
    assert!(
        report.bitmap && has_midx_bitmap(&repo),
        "the multi-pack repo gets a midx bitmap"
    );
    let (ok, stderr) = git(&repo, &["rev-list", "--test-bitmap", "main"]);
    assert!(ok, "canonical git accepts our midx bitmap: {stderr}");
}
