use std::path::Path;

use knot_git::{CommitRange, EntryKind, FileChange, Layout, LineCount, LogLimit, LogSkip, Repo};
use knot_types::{Listing, Oid, RefName, RepoDid, RepoPath};

fn rp(path: &str) -> RepoPath {
    RepoPath::new(path).unwrap()
}

mod common;
use common::{commit_file, git_ok as git};

#[test]
fn typed_reads_over_a_seeded_repo() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let bare = layout.create(&did).unwrap();
    let bare_path = layout.repo_path(&did).unwrap();
    let bare_str = bare_path.to_str().unwrap();

    let work_dir = tempfile::tempdir().unwrap();
    let work = work_dir.path();
    git(work, &["init", "-q", "-b", "main"]);
    commit_file(work, "a.txt", "one\n", "first");
    git(work, &["push", "-q", bare_str, "main"]);
    commit_file(work, "b.txt", "two\n", "second");
    std::fs::write(work.join("a.txt"), "one updated\n").unwrap();
    git(work, &["add", "-A"]);
    git(work, &["commit", "-q", "-m", "third"]);
    git(work, &["push", "-q", bare_str, "main"]);
    git(
        bare_path.as_path(),
        &["symbolic-ref", "HEAD", "refs/heads/main"],
    );

    let head = Oid::from_hex(&git(work, &["rev-parse", "HEAD"])).unwrap();
    let parent = Oid::from_hex(&git(work, &["rev-parse", "HEAD~1"])).unwrap();
    let root = Oid::from_hex(&git(work, &["rev-parse", "HEAD~2"])).unwrap();
    let tree = Oid::from_hex(&git(work, &["rev-parse", "HEAD^{tree}"])).unwrap();
    let blob_b = Oid::from_hex(&git(work, &["rev-parse", "HEAD:b.txt"])).unwrap();

    let head_ref = bare.head().expect("HEAD resolves");
    assert_eq!(head_ref.name.as_str(), "refs/heads/main");
    assert_eq!(head_ref.target, head);

    let commit = bare.find_commit(head).unwrap();
    assert_eq!(commit.id, head);
    assert_eq!(commit.tree, tree);
    assert_eq!(commit.parents, vec![parent]);
    assert_eq!(commit.author.name.as_str(), "nel");
    assert!(commit.message.starts_with("third"));

    let tree_entries = bare.find_tree(tree).unwrap();
    let names: Vec<&str> = tree_entries
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert!(names.contains(&"a.txt"));
    assert!(names.contains(&"b.txt"));
    assert!(
        tree_entries
            .entries
            .iter()
            .all(|entry| entry.kind == EntryKind::Blob)
    );

    assert_eq!(bare.read_blob(blob_b).unwrap(), b"two\n");

    let changes = bare.diff(CommitRange { base: root, head }).unwrap();
    assert!(changes.iter().any(
        |change| matches!(change, FileChange::Added { path, .. } if path.as_str() == "b.txt")
    ));
    assert!(changes.iter().any(
        |change| matches!(change, FileChange::Modified { path, .. } if path.as_str() == "a.txt")
    ));

    let comparison = bare.compare(CommitRange { base: root, head }).unwrap();
    assert_eq!(comparison.commits.len(), 2);
    assert!(comparison.commits.contains(&head));
    assert!(comparison.commits.contains(&parent));
    assert_eq!(comparison.changes, changes);
}

fn seed_main() -> (tempfile::TempDir, Layout, RepoDid, std::path::PathBuf, Oid) {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    layout.create(&did).unwrap();
    let bare_path = layout.repo_path(&did).unwrap();

    let work_dir = tempfile::tempdir().unwrap();
    let work = work_dir.path();
    git(work, &["init", "-q", "-b", "main"]);
    commit_file(work, "a.txt", "one\n", "first");
    git(work, &["push", "-q", bare_path.to_str().unwrap(), "main"]);
    let head = Oid::from_hex(&git(work, &["rev-parse", "HEAD"])).unwrap();
    (scan, layout, did, bare_path, head)
}

fn seed_rich() -> (tempfile::TempDir, tempfile::TempDir, Layout, RepoDid) {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    layout.create(&did).unwrap();
    let bare_path = layout.repo_path(&did).unwrap();

    let work_dir = tempfile::tempdir().unwrap();
    let work = work_dir.path();
    git(work, &["init", "-q", "-b", "main"]);
    commit_file(work, "a.txt", "one\ntwo\nthree\n", "first");
    std::fs::create_dir_all(work.join("src")).unwrap();
    commit_file(work, "src/lib.rs", "pub fn nel() {}\n", "add lib");
    commit_file(work, "a.txt", "one\ntwo\nthree\nfour\n", "extend a");
    git(work, &["tag", "light"]);
    git(work, &["tag", "-a", "v1.0.0", "-m", "release one"]);
    commit_file(
        work,
        "src/lib.rs",
        "pub fn nel() {}\npub fn teq() {}\n",
        "extend lib",
    );
    git(
        work,
        &["push", "-q", "--tags", bare_path.to_str().unwrap(), "main"],
    );
    git(
        bare_path.as_path(),
        &["symbolic-ref", "HEAD", "refs/heads/main"],
    );
    (scan, work_dir, layout, did)
}

#[test]
fn typed_reads_over_a_rich_repo() {
    let (_scan, work_dir, layout, did) = seed_rich();
    let work = work_dir.path();
    let bare = layout.open(&did).unwrap();
    let head = Oid::from_hex(&git(work, &["rev-parse", "HEAD"])).unwrap();
    let parent = Oid::from_hex(&git(work, &["rev-parse", "HEAD~1"])).unwrap();
    let base = Oid::from_hex(&git(work, &["rev-parse", "HEAD~2"])).unwrap();
    let root = Oid::from_hex(&git(work, &["rev-parse", "HEAD~3"])).unwrap();

    assert_eq!(bare.resolve_revision("main"), Some(head));
    assert_eq!(bare.resolve_revision("HEAD"), Some(head));
    assert_eq!(bare.resolve_revision(&head.to_hex()), Some(head));
    assert_eq!(bare.resolve_revision("does-not-exist"), None);
    let tag_object = bare.resolve_revision("v1.0.0").unwrap();
    assert_eq!(
        bare.peel_to_commit(tag_object).unwrap(),
        Oid::from_hex(&git(work, &["rev-parse", "v1.0.0^{commit}"])).unwrap(),
        "annotated tag peels to its commit"
    );

    let expected: Vec<String> = git(work, &["rev-list", "HEAD"])
        .lines()
        .map(str::to_string)
        .collect();
    let (walked, total) = bare
        .log_window(head, LogSkip::new(0), LogLimit::new(100))
        .unwrap();
    let walked: Vec<String> = walked.iter().map(|commit| commit.id.to_hex()).collect();
    assert_eq!(walked, expected, "log order matches git rev-list");
    assert_eq!(total, expected.len());

    let (page, total) = bare
        .log_window(head, LogSkip::new(1), LogLimit::new(2))
        .unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[0].id.to_hex(), expected[1]);
    assert_eq!(page[1].id.to_hex(), expected[2]);
    assert_eq!(total, expected.len(), "window still reports full count");

    let between: Vec<String> = bare
        .commits_between(CommitRange { base, head }, LogLimit::new(100))
        .unwrap()
        .iter()
        .map(|oid| oid.to_hex())
        .collect();
    let expected_between: Vec<String> =
        git(work, &["rev-list", &format!("{}..HEAD", base.to_hex())])
            .lines()
            .map(str::to_string)
            .collect();
    assert_eq!(between, expected_between);
    assert_eq!(
        bare.commits_between(CommitRange { base, head }, LogLimit::new(1))
            .unwrap()
            .len(),
        1,
        "walk stops at limit instead of collecting full range"
    );
    assert_eq!(bare.merge_base(head, base).unwrap(), Some(base));

    let commit = bare.find_commit(head).unwrap();
    assert_eq!(commit.author.name.as_str(), "nel");
    assert!(commit.pgp_signature.is_none());
    assert!(commit.extra_headers.is_empty());
    assert!(commit.change_id().is_none());

    let branches = bare.branch_list().unwrap();
    assert_eq!(branches.len(), 1);
    assert_eq!(branches[0].name.as_str(), "main");
    assert_eq!(branches[0].tip.id(), head);
    assert!(matches!(branches[0].tip, knot_git::BranchTip::Commit(_)));

    let tags = bare.tag_list().unwrap();
    assert_eq!(tags.len(), 2);
    let light = tags
        .iter()
        .find(|tag| tag.name.as_str() == "light")
        .unwrap();
    assert!(light.annotated.is_none());
    assert!(light.message.starts_with("extend a"));
    let annotated = tags
        .iter()
        .find(|tag| tag.name.as_str() == "v1.0.0")
        .unwrap();
    let detail = annotated.annotated.as_ref().unwrap();
    assert_eq!(annotated.message, "release one\n");
    assert_eq!(detail.tagger.as_ref().unwrap().name.as_str(), "nel");
    assert_eq!(
        detail.target,
        Oid::from_hex(&git(work, &["rev-parse", "v1.0.0^{commit}"])).unwrap()
    );
    assert_eq!(
        annotated.id,
        Oid::from_hex(&git(work, &["rev-parse", "v1.0.0"])).unwrap(),
        "tag info id is tag object itself"
    );

    let tree_root = bare.tree_entries_at(head, None).unwrap().unwrap();
    let names: Vec<&str> = tree_root.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, vec!["a.txt", "src"]);
    let a = tree_root
        .iter()
        .find(|entry| entry.name == "a.txt")
        .unwrap();
    assert_eq!(a.size, "one\ntwo\nthree\nfour\n".len() as u64);
    assert_eq!(a.kind.mode_octal(), "0100644");
    let src = tree_root.iter().find(|entry| entry.name == "src").unwrap();
    assert_eq!(src.kind, EntryKind::Tree);
    assert_eq!(src.size, 0);

    let sub = bare
        .tree_entries_at(head, Some(&rp("src")))
        .unwrap()
        .unwrap();
    assert_eq!(sub.len(), 1);
    assert_eq!(sub[0].name, "lib.rs");
    assert_eq!(
        bare.tree_entries_at(head, Some(&rp("a.txt")))
            .unwrap()
            .unwrap(),
        Vec::new(),
        "file path lists as empty"
    );
    assert!(
        bare.tree_entries_at(head, Some(&rp("missing")))
            .unwrap()
            .is_none()
    );
    assert!(RepoPath::new("../escape").is_err());

    let entry = bare.entry_at(head, &rp("src/lib.rs")).unwrap().unwrap();
    assert_eq!(
        entry.oid,
        Oid::from_hex(&git(work, &["rev-parse", "HEAD:src/lib.rs"])).unwrap()
    );

    let deadline = Some(std::time::Instant::now() + std::time::Duration::from_secs(10));
    let names: Vec<String> = tree_root.iter().map(|entry| entry.name.clone()).collect();
    let attributed = bare.last_commits(head, None, &names, deadline).unwrap();
    assert_eq!(
        attributed["a.txt"].id.to_hex(),
        git(work, &["log", "-1", "--format=%H", "--", "a.txt"])
    );
    assert_eq!(
        attributed["src"].id.to_hex(),
        git(work, &["log", "-1", "--format=%H", "--", "src"])
    );
    assert_eq!(attributed["a.txt"].subject, "extend a");
    let nested = bare
        .last_commits(head, Some(&rp("src")), &["lib.rs".to_string()], deadline)
        .unwrap();
    assert_eq!(
        nested["lib.rs"].id.to_hex(),
        git(work, &["log", "-1", "--format=%H", "--", "src/lib.rs"])
    );

    let patches = bare
        .commit_patches(knot_git::PatchRange {
            base: Some(parent),
            head,
        })
        .unwrap();
    assert_eq!(patches.len(), 1);
    let patch = &patches[0];
    assert_eq!(patch.path.as_str(), "src/lib.rs");
    assert_eq!(patch.status, knot_git::PatchStatus::Modified);
    assert!(!patch.is_binary);
    assert_eq!(patch.hunks.len(), 1);
    let hunk = &patch.hunks[0];
    assert_eq!(
        (
            hunk.old_start.get(),
            hunk.old_lines.get(),
            hunk.new_start.get(),
            hunk.new_lines.get()
        ),
        (1, 1, 1, 2)
    );
    assert_eq!(hunk.added(), LineCount::new(1));
    assert_eq!(hunk.deleted(), LineCount::new(0));
    assert_eq!(
        hunk.lines
            .iter()
            .map(|line| String::from_utf8_lossy(&line.text).into_owned())
            .collect::<Vec<_>>(),
        vec!["pub fn nel() {}\n", "pub fn teq() {}\n"]
    );

    let initial = bare
        .commit_patches(knot_git::PatchRange {
            base: None,
            head: root,
        })
        .unwrap();
    assert_eq!(initial.len(), 1);
    assert_eq!(initial[0].status, knot_git::PatchStatus::Added);
    assert_eq!(
        initial[0].hunks[0].old_start.get(),
        0,
        "added file hunk starts at -0,0"
    );
    assert_eq!(initial[0].hunks[0].old_lines.get(), 0);

    let tag_commit = bare.peel_to_commit(tag_object).unwrap();
    assert_eq!(
        bare.changed_paths(knot_git::PatchRange {
            base: None,
            head: tag_object,
        })
        .unwrap(),
        bare.changed_paths(knot_git::PatchRange {
            base: None,
            head: tag_commit,
        })
        .unwrap(),
        "an annotated tag peels to its commit before the trees are diffed"
    );
    let created = bare
        .changed_paths(knot_git::PatchRange { base: None, head })
        .unwrap();
    assert_eq!(
        created.paths(),
        [rp("a.txt"), rp("src/lib.rs")],
        "a ref creation lists every blob in the tree and no directory of them"
    );
    assert_eq!(created.listing(), Listing::Complete);
}

type TopoRow = (fn(&Path, &Oid), fn(&Repo, &Oid));

#[test]
fn ref_topology_reads_are_total() {
    let rows: &[TopoRow] = &[
        (
            |bare, _head| {
                git(bare, &["update-ref", "-d", "refs/heads/main"]);
            },
            |bare, _head| {
                assert!(bare.head().is_none());
                assert_eq!(bare.default_branch().unwrap().as_str(), "refs/heads/main");
                assert!(bare.references().unwrap().is_empty());
                assert!(bare.branches().unwrap().is_empty());
                assert!(bare.tags().unwrap().is_empty());
                assert!(bare.advertised_refs().unwrap().is_empty());
            },
        ),
        (
            |bare, head| {
                git(bare, &["update-ref", "--no-deref", "HEAD", &head.to_hex()]);
            },
            |bare, head| {
                assert!(bare.head().is_none());
                assert!(bare.default_branch().is_none());
                let branches = bare.branches().unwrap();
                assert_eq!(branches.len(), 1);
                assert_eq!(branches[0].target, *head);
                assert_eq!(
                    bare.find_ref(&RefName::new("refs/heads/main").unwrap())
                        .unwrap(),
                    Some(*head)
                );
            },
        ),
        (
            |bare, _head| {
                git(bare, &["symbolic-ref", "HEAD", "refs/heads/nursery"]);
            },
            |bare, _head| {
                assert!(bare.head().is_none());
                assert_eq!(
                    bare.default_branch().unwrap().as_str(),
                    "refs/heads/nursery"
                );
                let branches = bare.branches().unwrap();
                assert_eq!(branches.len(), 1);
                assert_eq!(branches[0].name.as_str(), "refs/heads/main");
            },
        ),
        (
            |bare, _head| {
                git(
                    bare,
                    &["symbolic-ref", "refs/heads/mirror", "refs/heads/gone"],
                );
            },
            |bare, head| {
                let refs = bare.references().unwrap();
                assert!(
                    refs.iter()
                        .all(|record| record.name.as_str() != "refs/heads/mirror"),
                    "symref to missing target must be dropped, not panic or resolve"
                );
                assert!(
                    refs.iter()
                        .any(|record| record.name.as_str() == "refs/heads/main"
                            && record.target == *head)
                );
                assert_eq!(
                    bare.find_ref(&RefName::new("refs/heads/mirror").unwrap())
                        .unwrap(),
                    None
                );
            },
        ),
        (
            |bare, _head| {
                git(bare, &["pack-refs", "--all"]);
                assert!(!bare.join("refs/heads/main").exists());
                assert!(bare.join("packed-refs").exists());
            },
            |bare, head| {
                let refs = bare.references().unwrap();
                assert!(
                    refs.iter()
                        .any(|record| record.name.as_str() == "refs/heads/main"
                            && record.target == *head)
                );
                assert_eq!(
                    bare.find_ref(&RefName::new("refs/heads/main").unwrap())
                        .unwrap(),
                    Some(*head)
                );
            },
        ),
    ];

    rows.iter().for_each(|(setup, check)| {
        let (_scan, layout, did, bare_path, head) = seed_main();
        setup(bare_path.as_path(), &head);
        let bare = layout.open(&did).unwrap();
        check(&bare, &head);
    });
}

#[test]
fn reachable_from_public_excludes_cob_only_commits() {
    let (_scan, work_dir, layout, did) = seed_rich();
    let work = work_dir.path();
    let bare_path = layout.repo_path(&did).unwrap();
    let bare = layout.open(&did).unwrap();

    let head = Oid::from_hex(&git(work, &["rev-parse", "HEAD"])).unwrap();
    let ancestor = Oid::from_hex(&git(work, &["rev-parse", "HEAD~2"])).unwrap();
    let tag_commit = Oid::from_hex(&git(work, &["rev-parse", "v1.0.0^{commit}"])).unwrap();
    assert!(bare.reachable_from_public(head).unwrap());
    assert!(bare.reachable_from_public(ancestor).unwrap());
    assert!(bare.reachable_from_public(tag_commit).unwrap());

    commit_file(work, "hidden.txt", "secret\n", "hidden");
    let hidden = Oid::from_hex(&git(work, &["rev-parse", "HEAD"])).unwrap();
    git(
        work,
        &[
            "push",
            "-q",
            bare_path.to_str().unwrap(),
            "HEAD:refs/cobs/sh.tangled.repo.collaborator/secret",
        ],
    );

    let bare = layout.open(&did).unwrap();
    assert!(bare.contains(hidden), "object lives in odb");
    assert!(
        !bare.reachable_from_public(hidden).unwrap(),
        "commit held only by cob ref isn't reachable from any public ref"
    );
}

#[test]
fn a_branch_tipped_by_a_tag_object_lists_opaquely() {
    let (_scan, work_dir, layout, did) = seed_rich();
    let work = work_dir.path();
    let bare_path = layout.repo_path(&did).unwrap();
    let tag_object = git(work, &["rev-parse", "v1.0.0"]);
    std::fs::write(
        bare_path.join("refs/heads/tagtip"),
        format!("{tag_object}\n"),
    )
    .unwrap();

    let bare = layout.open(&did).unwrap();
    let branches = bare.branch_list().unwrap();
    assert_eq!(branches.len(), 2);
    let tagtip = branches
        .iter()
        .find(|branch| branch.name.as_str() == "tagtip")
        .unwrap();
    match &tagtip.tip {
        knot_git::BranchTip::Opaque {
            id,
            message,
            created_at,
        } => {
            assert_eq!(*id, Oid::from_hex(&tag_object).unwrap());
            assert_eq!(message, "release one\n");
            assert!(created_at.get() > 0, "annotated tag records tagger time");
        }
        other => panic!("expected opaque tip, got {other:?}"),
    }
}

#[test]
fn extended_history_reads() {
    let (_scan, work_dir, layout, did) = seed_rich();
    let work = work_dir.path();
    let bare_path = layout.repo_path(&did).unwrap();

    let seed_head = git(work, &["rev-parse", "HEAD"]);
    git(
        work,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{seed_head},vendor/dep"),
        ],
    );
    git(work, &["commit", "-q", "-m", "add gitlink"]);
    git(work, &["push", "-q", bare_path.to_str().unwrap(), "main"]);
    let bare = layout.open(&did).unwrap();
    let linked = Oid::from_hex(&git(work, &["rev-parse", "HEAD"])).unwrap();
    assert!(
        bare.tree_entries_at(linked, Some(&rp("vendor/dep")))
            .unwrap()
            .is_none(),
        "submodule path isn't found"
    );
    assert!(
        bare.tree_entries_at(linked, Some(&rp("vendor")))
            .unwrap()
            .is_some(),
        "directory holding gitlink still lists"
    );

    commit_file(
        work,
        ".gitmodules",
        "# top comment\n[submodule \"kelp\"]\n\tpath = libs/kelp ; trailing comment\n\turl = \"https://oyster.cafe/kelp.git\"\n\tbranch = main\n[submodule \"whelk\"]\n\tpath = libs/whelk\n\turl = https://nel.pet/whelk.git # mirror\n",
        "add submodules",
    );
    git(work, &["push", "-q", bare_path.to_str().unwrap(), "main"]);
    let bare = layout.open(&did).unwrap();
    let with_mods = Oid::from_hex(&git(work, &["rev-parse", "HEAD"])).unwrap();
    let submodules = bare.submodules(with_mods).unwrap();
    assert_eq!(submodules.len(), 2);
    assert_eq!(submodules[0].name, "kelp");
    assert_eq!(submodules[0].path.as_str(), "libs/kelp");
    assert_eq!(submodules[0].url, "https://oyster.cafe/kelp.git");
    assert_eq!(
        submodules[0].branch,
        Some(knot_types::BranchName::new("main").unwrap())
    );
    assert_eq!(submodules[1].branch, None);

    std::fs::write(work.join("blob.bin"), [0u8, 159, 146, 150, 0, 1]).unwrap();
    std::fs::write(work.join("noeol.txt"), "no newline at end").unwrap();
    git(work, &["add", "-A"]);
    git(work, &["commit", "-q", "-m", "binary and noeol"]);
    git(work, &["push", "-q", bare_path.to_str().unwrap(), "main"]);
    let bare = layout.open(&did).unwrap();
    let head = Oid::from_hex(&git(work, &["rev-parse", "HEAD"])).unwrap();
    let parent = Oid::from_hex(&git(work, &["rev-parse", "HEAD~1"])).unwrap();
    let patches = bare
        .commit_patches(knot_git::PatchRange {
            base: Some(parent),
            head,
        })
        .unwrap();
    let binary = patches
        .iter()
        .find(|patch| patch.path.as_str() == "blob.bin")
        .unwrap();
    assert!(binary.is_binary);
    assert!(binary.hunks.is_empty());
    let noeol = patches
        .iter()
        .find(|patch| patch.path.as_str() == "noeol.txt")
        .unwrap();
    let last = noeol.hunks[0].lines.last().unwrap();
    assert_eq!(last.text, b"no newline at end".to_vec());
}

#[test]
fn archives_round_trip_through_tar() {
    let (_scan, work_dir, layout, did) = seed_rich();
    let work = work_dir.path();
    let bare = layout.open(&did).unwrap();
    let head = Oid::from_hex(&git(work, &["rev-parse", "HEAD"])).unwrap();
    let tree = bare.peel_to_tree(head).unwrap();

    let mut out = std::io::Cursor::new(Vec::new());
    bare.write_archive(
        tree,
        knot_git::ArchiveFormat::TarGz,
        Some(&knot_git::ArchivePrefix::new("squid-main/").unwrap()),
        &mut out,
    )
    .unwrap();
    let compressed = out.into_inner();
    assert_eq!(
        &compressed[..2],
        &[0x1f, 0x8b],
        "tar.gz starts with gzip magic"
    );

    let mut decoder = flate2::read::GzDecoder::new(compressed.as_slice());
    let mut tar = Vec::new();
    std::io::Read::read_to_end(&mut decoder, &mut tar).unwrap();
    let needle = b"squid-main/src/lib.rs";
    assert!(
        tar.windows(needle.len()).any(|window| window == needle),
        "tar contains prefixed entries"
    );
}

#[test]
fn a_filename_with_a_backslash_is_addressable() {
    let (_scan, layout, did, bare_path, _head) = seed_main();
    let work_dir = tempfile::tempdir().unwrap();
    let work = work_dir.path();
    git(
        work,
        &["clone", "-q", bare_path.to_str().unwrap(), "checkout"],
    );
    let clone = work.join("checkout");
    commit_file(&clone, "back\\slash.txt", "escaped\n", "backslash name");
    git(&clone, &["push", "-q", "origin", "main"]);

    let bare = layout.open(&did).unwrap();
    let head = Oid::from_hex(&git(&clone, &["rev-parse", "HEAD"])).unwrap();
    let entry = bare
        .entry_at(head, &rp("back\\slash.txt"))
        .unwrap()
        .expect("backslash in filename is legal and addressable");
    assert_eq!(bare.read_blob(entry.oid).unwrap(), b"escaped\n");
}

#[test]
fn an_oversized_blob_diffs_as_binary_without_loading_it() {
    let (_scan, layout, did, bare_path, _head) = seed_main();
    let work_dir = tempfile::tempdir().unwrap();
    let work = work_dir.path();
    git(
        work,
        &["clone", "-q", bare_path.to_str().unwrap(), "checkout"],
    );
    let clone = work.join("checkout");
    let oversized = vec![b'a'; (knot_git::MAX_DIFF_BLOB_BYTES + 1) as usize];
    std::fs::write(clone.join("huge.txt"), &oversized).unwrap();
    git(&clone, &["add", "-A"]);
    git(&clone, &["commit", "-q", "-m", "huge text file"]);
    git(&clone, &["push", "-q", "origin", "main"]);

    let bare = layout.open(&did).unwrap();
    let head = Oid::from_hex(&git(&clone, &["rev-parse", "HEAD"])).unwrap();
    let parent = Oid::from_hex(&git(&clone, &["rev-parse", "HEAD~1"])).unwrap();
    let patches = bare
        .commit_patches(knot_git::PatchRange {
            base: Some(parent),
            head,
        })
        .unwrap();
    let huge = patches
        .iter()
        .find(|patch| patch.path.as_str() == "huge.txt")
        .unwrap();
    assert!(
        huge.is_binary,
        "blob past diff budget falls back to binary instead of being loaded"
    );
    assert!(huge.hunks.is_empty());
}
