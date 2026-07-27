use std::collections::HashMap;
use std::path::Path;

use knot_git::Repo;
use knot_maintenance::run_repo;

mod common;
use common::{git_available, now, options};

fn skip() -> bool {
    if git_available() {
        return false;
    }
    eprintln!("skipping commit-graph differential: git unavailable");
    true
}

fn git_at(dir: &Path, date: i64, args: &[&str]) {
    let stamp = format!("{date} +0000");
    let out = knot_fixtures::command_at(dir, &stamp)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git(dir: &Path, args: &[&str]) {
    git_at(dir, 1_700_000_000, args);
}

fn write_file(dir: &Path, rel: &str, contents: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn commit(dir: &Path, rel: &str, contents: &str, message: &str) {
    write_file(dir, rel, contents);
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-m", message]);
}

fn seed_history(dir: &Path, format: &str) {
    git(dir, &["init", "--object-format", format, "-b", "main"]);
    write_file(dir, "dir1/a.txt", "a1");
    write_file(dir, "dir1/sub/b.txt", "b1");
    commit(dir, "c.txt", "c1", "root");
    commit(dir, "dir1/sub/b.txt", "b2", "edit nested");
    git(dir, &["checkout", "-b", "feature"]);
    commit(dir, "c.txt", "c2", "feature edit");
    git(dir, &["checkout", "main"]);
    commit(dir, "dir2/d.txt", "d1", "add dir2");
    git(dir, &["merge", "--no-ff", "-m", "merge feature", "feature"]);
    git(dir, &["tag", "v1"]);
}

fn read_chunks(bytes: &[u8]) -> HashMap<[u8; 4], Vec<u8>> {
    let count = bytes[6] as usize;
    let table = &bytes[8..8 + (count + 1) * 12];
    let entry = |index: usize| -> ([u8; 4], u64) {
        let base = index * 12;
        let id = table[base..base + 4].try_into().unwrap();
        (
            id,
            u64::from_be_bytes(table[base + 4..base + 12].try_into().unwrap()),
        )
    };
    (0..count)
        .map(|index| {
            let (id, start) = entry(index);
            let (_, end) = entry(index + 1);
            (id, bytes[start as usize..end as usize].to_vec())
        })
        .collect()
}

fn graph_verify(root: &Path) -> (bool, String) {
    let out = knot_fixtures::command(root)
        .args(["commit-graph", "verify"])
        .output()
        .expect("git commit-graph verify runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn path_log(dir: &Path, path: &str, use_graph: bool) -> String {
    let out = knot_fixtures::command(dir)
        .args([
            "-c",
            &format!("core.commitGraph={use_graph}"),
            "-c",
            "commitGraph.readChangedPaths=true",
            "log",
            "--format=%H",
            "--",
            path,
        ])
        .output()
        .expect("git log runs");
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap()
}

fn build_and_compare(root: &Path, format: &str, chunks: &[&[u8; 4]]) {
    git(
        root,
        &[
            "-c",
            "commitGraph.changedPathsVersion=2",
            "commit-graph",
            "write",
            "--reachable",
            "--changed-paths",
        ],
    );
    let graph_file = root.join(".git/objects/info/commit-graph");
    let canonical = read_chunks(&std::fs::read(&graph_file).unwrap());

    assert!(
        run_repo(&Repo::open(root).unwrap(), now(), &options())
            .unwrap()
            .commit_graph,
        "knot wrote a graph ({format})"
    );
    let ours = read_chunks(&std::fs::read(&graph_file).unwrap());
    chunks.iter().for_each(|id| {
        assert_eq!(
            ours.get(*id),
            canonical.get(*id),
            "chunk {} differs ({format})",
            String::from_utf8_lossy(*id)
        );
    });
    let (ok, stderr) = graph_verify(root);
    assert!(ok, "git commit-graph verify failed ({format}): {stderr}");
}

fn check_against_git(format: &str) {
    if skip() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    seed_history(root, format);
    build_and_compare(root, format, &[b"OIDL", b"CDAT", b"GDA2", b"BIDX", b"BDAT"]);

    ["dir1/sub/b.txt", "dir1/sub", "dir1", "c.txt", "dir2/d.txt"]
        .iter()
        .for_each(|path| {
            assert_eq!(
                path_log(root, path, true),
                path_log(root, path, false),
                "changed-path bloom altered `git log -- {path}` ({format})"
            );
        });
}

#[test]
fn matches_canonical_git_sha1() {
    check_against_git("sha1");
}

#[test]
fn matches_canonical_git_sha256() {
    check_against_git("sha256");
}

#[test]
fn writing_the_graph_clears_a_pre_existing_split_chain() {
    if skip() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    seed_history(root, "sha1");

    git(root, &["commit-graph", "write", "--reachable", "--split"]);
    let chain = root.join(".git/objects/info/commit-graphs");
    assert!(chain.exists(), "git wrote a split commit-graph chain");

    assert!(
        run_repo(&Repo::open(root).unwrap(), now(), &options())
            .unwrap()
            .commit_graph,
        "knot wrote a monolithic graph"
    );
    assert!(
        !chain.exists(),
        "the stale split chain is removed so it cannot shadow the fresh graph"
    );
    assert!(root.join(".git/objects/info/commit-graph").exists());
    let (ok, stderr) = graph_verify(root);
    assert!(
        ok,
        "git verify passes after the chain is replaced: {stderr}"
    );
}

#[test]
fn large_filter_sentinel_matches_git_when_dirs_overflow_the_limit() {
    if skip() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "--object-format", "sha1", "-b", "main"]);
    (0..200).for_each(|i| write_file(root, &format!("a{i}/b{i}/c.txt"), "x"));
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "wide refactor"]);
    build_and_compare(root, "sha1", &[b"BIDX", b"BDAT"]);
}

#[test]
fn corrected_date_overflow_matches_git() {
    if skip() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "--object-format", "sha1", "-b", "main"]);
    write_file(root, "a.txt", "1");
    git_at(root, 4_000_000_000, &["add", "-A"]);
    git_at(root, 4_000_000_000, &["commit", "-m", "far-future root"]);
    write_file(root, "a.txt", "2");
    git_at(root, 1_000_000_000, &["add", "-A"]);
    git_at(root, 1_000_000_000, &["commit", "-m", "past child"]);
    build_and_compare(root, "sha1", &[b"CDAT", b"GDA2", b"GDO2"]);
}

#[test]
fn changed_path_filter_matches_git_for_high_byte_names() {
    if skip() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "--object-format", "sha1", "-b", "main"]);
    write_file(root, "café/résumé.txt", "1");
    commit(root, "naïve.md", "2", "non-ascii paths");
    commit(root, "café/résumé.txt", "2", "edit non-ascii");
    build_and_compare(root, "sha1", &[b"BIDX", b"BDAT"]);
}
