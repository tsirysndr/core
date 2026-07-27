#![allow(dead_code, unused_imports)]

use std::collections::BTreeSet;
use std::ops::Range;
use std::path::Path;

use knot_git::{
    EntryKind, Identity, Layout, NewCommit, RefUpdate, Repo, StagedAction, StagedChange,
};
use knot_maintenance::{GeometricFactor, ObjectCount, Options, PruneGrace, ReflogRetention};
use knot_types::{AuthorName, BranchName, Email, ObjectFormat, Oid, RefName, RepoDid, UnixSeconds};

pub const EMPTY_TREE_SHA1: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
pub const EMPTY_TREE_SHA256: &str =
    "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";

pub fn empty_tree(format: ObjectFormat) -> &'static str {
    if format == ObjectFormat::SHA256 {
        EMPTY_TREE_SHA256
    } else {
        EMPTY_TREE_SHA1
    }
}

pub fn now() -> UnixSeconds {
    UnixSeconds::new(1_700_000_500)
}

pub use knot_fixtures::available as git_available;

pub fn identity() -> Identity {
    Identity {
        name: AuthorName::new("nel"),
        email: Email::new("nel@oyster.cafe"),
        time: UnixSeconds::new(1_700_000_000),
        offset_seconds: 0,
    }
}

pub fn options() -> Options {
    Options {
        repack_max_objects: ObjectCount::new(1_000_000),
        geometric_factor: GeometricFactor::full_repack(),
        prune_grace: PruneGrace::from_secs(0),
        reflog_floor: ReflogRetention::from_secs(i64::MAX as u64 / 4),
        commit_graph: true,
        multi_pack_index: true,
        bitmap: true,
    }
}

pub fn create_repo(scan: &Path, format: ObjectFormat, did: &str) -> Repo {
    Layout::new(scan)
        .with_object_format(format)
        .with_default_branch(BranchName::new("main").unwrap())
        .create(&RepoDid::new(did).unwrap())
        .unwrap()
}

pub fn commit_on(repo: &Repo, empty_tree: &str, body: u8, parents: Vec<Oid>) -> Oid {
    let tree = repo
        .write_staged_tree(
            Oid::from_hex(empty_tree).unwrap(),
            &[StagedChange {
                path: knot_types::RepoPath::new(format!("file{body}.txt")).unwrap(),
                action: StagedAction::Put {
                    content: vec![body, body, body],
                    kind: EntryKind::Blob,
                },
            }],
        )
        .unwrap();
    repo.write_commit(&NewCommit {
        tree,
        parents,
        author: identity(),
        committer: identity(),
        message: format!("commit {body}"),
        extra_headers: Vec::new(),
    })
    .unwrap()
}

pub fn commit(repo: &Repo, body: u8, parents: Vec<Oid>) -> Oid {
    commit_on(repo, EMPTY_TREE_SHA1, body, parents)
}

pub fn chain(repo: &Repo, empty_tree: &str, bodies: Range<u8>, start: Option<Oid>) -> Oid {
    bodies
        .fold(start, |parent, body| {
            let parents = parent.map(|tip| vec![tip]).unwrap_or_default();
            Some(commit_on(repo, empty_tree, body, parents))
        })
        .expect("a non-empty body range yields a tip")
}

pub fn set_ref(repo: &Repo, name: &str, new: Oid) {
    let refname = RefName::new(name).unwrap();
    let update = match repo.find_ref(&refname).unwrap() {
        Some(old) => RefUpdate::Update {
            name: refname,
            old,
            new,
        },
        None => RefUpdate::Create { name: refname, new },
    };
    repo.update_ref(&update).unwrap();
}

pub fn delete_ref(repo: &Repo, name: &str) {
    let refname = RefName::new(name).unwrap();
    let old = repo.find_ref(&refname).unwrap().unwrap();
    repo.update_ref(&RefUpdate::Delete { name: refname, old })
        .unwrap();
}

pub fn set_reflog_seconds(repo: &Repo, name: &str, seconds: &[i64]) {
    let path = repo.git().git_dir().join("logs").join(name);
    let text = std::fs::read_to_string(&path).expect("reflog file exists");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines.len(),
        seconds.len(),
        "set_reflog_seconds needs one timestamp per reflog line"
    );
    let rewritten = lines
        .iter()
        .zip(seconds)
        .map(|(line, secs)| rewrite_reflog_seconds(line, *secs))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, format!("{rewritten}\n")).expect("rewrite reflog");
}

fn rewrite_reflog_seconds(line: &str, secs: i64) -> String {
    let (meta, message) = line
        .split_once('\t')
        .expect("reflog line has a message tab");
    let tokens: Vec<&str> = meta.split(' ').collect();
    let tz = tokens.last().expect("reflog line has a timezone");
    let head = tokens[..tokens.len() - 2].join(" ");
    format!("{head} {secs} {tz}\t{message}")
}

pub fn git(repo: &Repo, args: &[&str]) -> (bool, String) {
    let out = knot_fixtures::command(repo.git().git_dir())
        .args(args)
        .output()
        .expect("git runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

pub fn fsck(repo: &Repo) -> (bool, String) {
    git(repo, &["fsck", "--no-dangling", "--no-progress"])
}

pub fn fsck_clean(repo: &Repo) -> bool {
    fsck(repo).0
}

pub fn assert_fsck_clean(repo: &Repo) {
    let (clean, stderr) = fsck(repo);
    assert!(clean, "fsck failed: {stderr}");
}

pub fn reachable_objects(repo: &Repo) -> BTreeSet<String> {
    let out = knot_fixtures::command(repo.git().git_dir())
        .args(["rev-list", "--objects", "--all"])
        .output()
        .expect("git rev-list runs");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect()
}

pub fn midx_verifies(repo: &Repo) -> Option<(bool, String)> {
    let midx = repo.git().git_dir().join("objects/pack/multi-pack-index");
    midx.exists()
        .then(|| git(repo, &["multi-pack-index", "verify"]))
}

fn pack_entries(repo: &Repo) -> impl Iterator<Item = std::path::PathBuf> {
    std::fs::read_dir(repo.git().git_dir().join("objects/pack"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
}

pub fn idx_stems(repo: &Repo) -> BTreeSet<String> {
    pack_entries(repo)
        .filter(|path| path.extension().is_some_and(|ext| ext == "idx"))
        .filter_map(|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string)
        })
        .collect()
}

pub fn has_cruft_pack(repo: &Repo) -> bool {
    pack_entries(repo).any(|path| path.extension().is_some_and(|ext| ext == "mtimes"))
}

pub fn has_bitmap(repo: &Repo) -> bool {
    pack_entries(repo).any(|path| path.extension().is_some_and(|ext| ext == "bitmap"))
}

pub fn has_midx_bitmap(repo: &Repo) -> bool {
    pack_entries(repo)
        .filter_map(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
        })
        .any(|name| name.starts_with("multi-pack-index-") && name.ends_with(".bitmap"))
}
