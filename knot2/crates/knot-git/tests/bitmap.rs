use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use knot_git::{
    Haves, Repo, Wants, reachable_via_bitmap, verbatim_clone_pack, write_bitmap, write_midx_bitmap,
};
use knot_types::Oid;

mod common;
use common::{commit_file as commit, git, git_available, git_ok as ok};

fn test_bitmap(path: &Path) {
    let (ok, report) = git(path, &["rev-list", "--test-bitmap", "HEAD"]);
    assert!(ok, "git rejected the bitmap: {report}");
}

fn seed(format: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    ok(
        path,
        &[
            "init",
            "-q",
            "--object-format",
            format,
            "--initial-branch",
            "main",
        ],
    );
    commit(path, "a.txt", "alpha\n", "root");
    commit(path, "b.txt", "beta\n", "second");
    ok(path, &["checkout", "-q", "-b", "feature"]);
    commit(path, "c.txt", "gamma\n", "feature work");
    ok(path, &["checkout", "-q", "main"]);
    commit(path, "d.txt", "delta\n", "more main");
    ok(path, &["tag", "-a", "v1", "-m", "release one"]);
    ok(path, &["repack", "-adq"]);
    dir
}

fn find_pack(path: &Path, suffix: &str) -> PathBuf {
    std::fs::read_dir(path.join(".git/objects/pack"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(suffix))
        })
        .unwrap_or_else(|| panic!("a pack file ending {suffix}"))
}

fn idx_count(path: &Path) -> usize {
    std::fs::read_dir(path.join(".git/objects/pack"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "idx"))
        .count()
}

fn rev_parse(path: &Path, name: &str) -> Oid {
    Oid::from_hex(ok(path, &["rev-parse", name]).trim()).unwrap()
}

fn rev_list(path: &Path, revs: &[&str]) -> HashSet<Oid> {
    let args: Vec<&str> = ["rev-list", "--objects"]
        .into_iter()
        .chain(revs.iter().copied())
        .collect();
    ok(path, &args)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter_map(|token| Oid::from_hex(token).ok())
        .collect()
}

fn closure(repo: &Repo, wants: &[Oid], haves: &[Oid]) -> HashSet<Oid> {
    reachable_via_bitmap(repo, Wants::new(wants), Haves::new(haves))
        .unwrap()
        .expect("bitmap fast path resolves")
        .into_iter()
        .collect()
}

fn walked(repo: &Repo, want: Oid) -> HashSet<Oid> {
    repo.select_pack_objects(Wants::new(&[want]), Haves::new(&[]))
        .unwrap()
        .into_iter()
        .collect()
}

fn reject_corruption(repo: &Repo, bitmap: &Path, head: Oid) {
    let intact = std::fs::read(bitmap).unwrap();
    let mut flipped = intact.clone();
    *flipped.last_mut().unwrap() ^= 0xff;
    std::fs::write(bitmap, &flipped).unwrap();
    assert!(
        reachable_via_bitmap(repo, Wants::new(&[head]), Haves::new(&[])).is_err(),
        "a corrupt checksum is rejected"
    );
    std::fs::write(bitmap, &intact[..intact.len() / 2]).unwrap();
    assert!(
        reachable_via_bitmap(repo, Wants::new(&[head]), Haves::new(&[])).is_err(),
        "a truncated bitmap is rejected without a panic"
    );
}

fn run_lifecycle(format: &str) {
    if !git_available() {
        eprintln!("skipping bitmap lifecycle: git unavailable");
        return;
    }
    let dir = seed(format);
    let path = dir.path();
    let repo = Repo::open(path).unwrap();
    let idx = find_pack(path, ".idx");
    assert!(
        write_bitmap(&repo, &idx).unwrap(),
        "a single-pack repo gets a bitmap"
    );
    test_bitmap(path);

    let head = rev_parse(path, "HEAD");
    let ours = closure(&repo, &[head], &[]);
    assert_eq!(
        ours,
        rev_list(path, &["HEAD"]),
        "closure equals rev-list --objects HEAD"
    );
    assert_eq!(ours, walked(&repo, head), "closure equals the walk closure");
    assert_eq!(
        ours,
        closure(&repo, &vec![head; 2048], &[]),
        "duplicate wants fold to one closure"
    );

    let feature = rev_parse(path, "feature");
    let main = rev_parse(path, "main");
    assert_eq!(
        closure(&repo, &[feature], &[main]),
        rev_list(path, &["feature", "^main"]),
        "feature minus main"
    );

    let tag = rev_parse(path, "v1");
    let mut whole = verbatim_clone_pack(&repo, Wants::new(&[main, feature, tag]))
        .unwrap()
        .expect("full clone reuses whole pack");
    let mut header = [0u8; 12];
    whole.read_exact(&mut header).unwrap();
    assert_eq!(
        &header[..4],
        b"PACK",
        "verbatim reuse returns the on-disk pack"
    );
    let count = u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize;
    assert_eq!(
        count,
        rev_list(path, &["--all"]).len(),
        "verbatim reuse streams every object"
    );
    assert!(
        verbatim_clone_pack(&repo, Wants::new(&[main]))
            .unwrap()
            .is_none(),
        "a partial closure never reuses the whole pack"
    );

    ok(path, &["tag", "-a", "v2", "-m", "release two", "HEAD"]);
    let v2 = rev_parse(path, "v2");
    assert_ne!(v2, head);
    assert!(
        reachable_via_bitmap(&repo, Wants::new(&[v2]), Haves::new(&[]))
            .unwrap()
            .is_none(),
        "a want outside the bitmapped pack fails closed"
    );
    assert!(
        walked(&repo, v2).contains(&v2),
        "the fallback walk includes the wanted tag"
    );

    std::fs::remove_file(idx.with_extension("bitmap")).unwrap();
    commit(path, "e.txt", "epsilon\n", "post-bitmap growth");
    commit(path, "f.txt", "zeta\n", "more growth");
    ok(path, &["repack", "-dq"]);
    ok(path, &["multi-pack-index", "write"]);
    assert!(idx_count(path) >= 2, "the repo now spans multiple packs");

    let repo = Repo::open(path).unwrap();
    assert!(
        write_midx_bitmap(&repo).unwrap(),
        "a multi-pack repo gets a midx bitmap"
    );
    test_bitmap(path);
    let head = rev_parse(path, "HEAD");
    assert_eq!(
        closure(&repo, &[head], &[]),
        rev_list(path, &["HEAD"]),
        "midx closure equals rev-list HEAD"
    );

    reject_corruption(&repo, &find_pack(path, ".bitmap"), head);
}

#[test]
fn bitmap_single_pack_then_midx_round_trips_against_canonical_git_sha1() {
    run_lifecycle("sha1");
}

#[test]
fn bitmap_single_pack_then_midx_round_trips_against_canonical_git_sha256() {
    run_lifecycle("sha256");
}
