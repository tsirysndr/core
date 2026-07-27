use std::path::Path;

use knot_git::{
    ApplyOutcome, ConflictReason, Identity, Layout, NewCommit, PatchApplier, RefUpdate, Repo,
    is_format_patch, parse_mailbox, parse_patch,
};
use knot_types::{AuthorName, Email, Oid, RefName, RepoDid, UnixSeconds};

mod common;
use common::{git_ok as git, seeded};

fn stage_and_commit(work: &Path, message: &str) {
    git(work, &["add", "-A"]);
    git(work, &["commit", "-q", "-m", message]);
}

fn push_main(work: &Path, layout: &Layout, did: &RepoDid) {
    let bare = layout.repo_path(did).unwrap();
    git(work, &["push", "-q", bare.to_str().unwrap(), "main"]);
    git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
}

fn main_ref() -> RefName {
    RefName::new("refs/heads/main").unwrap()
}

fn committer() -> Identity {
    Identity {
        name: AuthorName::new("Tangled"),
        email: Email::new("noreply@tangled.sh"),
        time: UnixSeconds::new(1_700_000_000),
        offset_seconds: 0,
    }
}

fn apply_mailbox_natively(bare: &Repo, patch: &str) -> Vec<Oid> {
    let mails = parse_mailbox(patch).unwrap();
    let tip = bare.find_ref(&main_ref()).unwrap().unwrap();
    let base_tree = bare.find_commit(tip).unwrap().tree;
    let mut applier = PatchApplier::new(bare, tip);
    let (commits, _, new_tip) = mails.iter().fold(
        (Vec::new(), base_tree, tip),
        |(mut commits, tree, parent), mail| {
            let staged = match applier.step(&mail.files).unwrap() {
                ApplyOutcome::Clean(staged) => staged,
                ApplyOutcome::Conflicted(conflicts) => {
                    panic!("expected clean apply, got conflicts {conflicts:?}")
                }
            };
            let next_tree = bare.write_staged_tree(tree, &staged).unwrap();
            let commit = bare
                .write_commit(&NewCommit {
                    tree: next_tree,
                    parents: vec![parent],
                    author: Identity {
                        name: mail.author_name.clone(),
                        email: mail.author_email.clone(),
                        time: UnixSeconds::new(1_700_000_000),
                        offset_seconds: 0,
                    },
                    committer: committer(),
                    message: mail.commit_message(),
                    extra_headers: mail
                        .change_id
                        .iter()
                        .map(|id| ("change-id".to_string(), id.as_str().as_bytes().to_vec()))
                        .collect(),
                })
                .unwrap();
            commits.push(commit);
            (commits, next_tree, commit)
        },
    );
    bare.update_ref(&RefUpdate::Update {
        name: main_ref(),
        old: tip,
        new: new_tip,
    })
    .unwrap();
    commits
}

#[test]
fn a_format_patch_series_applies_tree_identical_to_git_am() {
    let (_scan, work_dir, layout, did) = seeded();
    let work = work_dir.path();

    let blob: Vec<u8> = (0u32..8192).map(|i| (i * 31 % 251) as u8).collect();
    std::fs::write(work.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
    std::fs::create_dir_all(work.join("sub")).unwrap();
    std::fs::write(work.join("sub/inner.txt"), "nested\n").unwrap();
    std::fs::write(work.join("noeol.txt"), "tail without newline").unwrap();
    std::fs::write(work.join("data.bin"), &blob).unwrap();
    std::fs::write(work.join("drop.bin"), [0u8, 1, 2, 3, 0, 9]).unwrap();
    stage_and_commit(work, "base");
    push_main(work, &layout, &did);
    let base = git(work, &["rev-parse", "HEAD"]);

    std::fs::write(work.join("a.txt"), "alpha\nBETA\ngamma\n").unwrap();
    stage_and_commit(work, "first subject\n\nfirst body line");

    git(work, &["mv", "a.txt", "moved.txt"]);
    std::fs::write(work.join("moved.txt"), "alpha\nBETA\ngamma\ndelta\n").unwrap();
    std::fs::write(work.join("run.sh"), "#!/bin/sh\necho reef\n").unwrap();
    git(work, &["add", "-A"]);
    git(work, &["update-index", "--chmod=+x", "run.sh"]);
    git(work, &["commit", "-q", "-m", "second"]);

    std::fs::remove_file(work.join("noeol.txt")).unwrap();
    std::fs::write(work.join("sp ace.txt"), "spaced\n").unwrap();
    std::fs::write(work.join("café.txt"), "unicode\n").unwrap();
    stage_and_commit(work, "third");

    let mutated: Vec<u8> = blob
        .iter()
        .copied()
        .chain([0u8, 255, 254, 7])
        .map(|byte| match byte {
            42 => 24,
            other => other,
        })
        .collect();
    std::fs::write(work.join("data.bin"), &mutated).unwrap();
    std::fs::remove_file(work.join("drop.bin")).unwrap();
    std::fs::write(work.join("new.bin"), [9u8, 0, 8, 0, 7]).unwrap();
    stage_and_commit(work, "binary churn");

    let patch = git(
        work,
        &["format-patch", "--stdout", &format!("{base}..HEAD")],
    );
    assert!(is_format_patch(&patch));
    assert!(patch.contains("GIT binary patch"));

    let bare = layout.open(&did).unwrap();
    let ours = apply_mailbox_natively(&bare, &patch);

    let expected: Vec<String> = git(work, &["rev-list", "--reverse", &format!("{base}..HEAD")])
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(ours.len(), expected.len());
    ours.iter().zip(&expected).for_each(|(our_oid, real)| {
        let our_commit = bare.find_commit(*our_oid).unwrap();
        let real_tree =
            Oid::from_hex(&git(work, &["rev-parse", &format!("{real}^{{tree}}")])).unwrap();
        assert_eq!(
            our_commit.tree, real_tree,
            "natively applied tree must be byte-identical to git am's"
        );
        let real_message = git(work, &["log", "-1", "--format=%B", real]);
        assert_eq!(our_commit.message.trim_end(), real_message.trim_end());
        assert_eq!(our_commit.author.name.as_str(), "nel");
        assert_eq!(our_commit.author.email.as_str(), "nel@oyster.cafe");
        assert_eq!(our_commit.committer.name.as_str(), "Tangled");
        assert_eq!(our_commit.committer.email.as_str(), "noreply@tangled.sh");
    });
    assert_eq!(
        bare.find_ref(&main_ref()).unwrap(),
        Some(*ours.last().unwrap())
    );

    git(work, &["checkout", "-q", "-b", "subline"]);
    std::fs::write(work.join("seed.txt"), "one\n").unwrap();
    stage_and_commit(work, "seed one");
    let old_oid = git(work, &["rev-parse", "HEAD"]);
    std::fs::write(work.join("seed.txt"), "two\n").unwrap();
    stage_and_commit(work, "seed two");
    let new_oid = git(work, &["rev-parse", "HEAD"]);
    git(
        work,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{old_oid},vendor"),
        ],
    );
    git(work, &["commit", "-q", "-m", "add submodule"]);
    let sub_base = git(work, &["rev-parse", "HEAD"]);
    let bare_path = layout.repo_path(&did).unwrap();
    git(
        work,
        &[
            "push",
            "-q",
            "-f",
            bare_path.to_str().unwrap(),
            &format!("{sub_base}:refs/heads/main"),
        ],
    );

    git(
        work,
        &[
            "update-index",
            "--cacheinfo",
            &format!("160000,{new_oid},vendor"),
        ],
    );
    git(work, &["commit", "-q", "-m", "bump submodule"]);
    let bump = git(
        work,
        &["format-patch", "--stdout", &format!("{sub_base}..HEAD")],
    );
    assert!(bump.contains("Subproject commit"));

    let bumped = layout.open(&did).unwrap();
    let applied = apply_mailbox_natively(&bumped, &bump);
    let real_tree = Oid::from_hex(&git(work, &["rev-parse", "HEAD^{tree}"])).unwrap();
    assert_eq!(bumped.find_commit(applied[0]).unwrap().tree, real_tree);
}

#[test]
fn a_unified_diff_applies_tree_identical_to_git_apply() {
    let (_scan, work_dir, layout, did) = seeded();
    let work = work_dir.path();

    std::fs::write(work.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    std::fs::write(work.join("gone.txt"), "doomed\n").unwrap();
    std::fs::write(work.join("noeol.txt"), "no newline here").unwrap();
    stage_and_commit(work, "base");
    push_main(work, &layout, &did);
    let base = git(work, &["rev-parse", "HEAD"]);

    std::fs::write(work.join("a.txt"), "one\nTWO\nthree\nfour\n").unwrap();
    std::fs::remove_file(work.join("gone.txt")).unwrap();
    std::fs::write(work.join("fresh.txt"), "brand new\n").unwrap();
    std::fs::write(work.join("noeol.txt"), "still no newline").unwrap();
    stage_and_commit(work, "changes");

    let patch = git(work, &["diff", &base, "HEAD"]);
    assert!(!is_format_patch(&patch));
    let files = parse_patch(&patch).unwrap();

    let bare = layout.open(&did).unwrap();
    let tip = bare.find_ref(&main_ref()).unwrap().unwrap();
    let base_tree = bare.find_commit(tip).unwrap().tree;
    let mut applier = PatchApplier::new(&bare, tip);
    let staged = match applier.step(&files).unwrap() {
        ApplyOutcome::Clean(staged) => staged,
        ApplyOutcome::Conflicted(conflicts) => panic!("unexpected conflicts {conflicts:?}"),
    };
    let our_tree = bare.write_staged_tree(base_tree, &staged).unwrap();

    let real_tree = Oid::from_hex(&git(work, &["rev-parse", "HEAD^{tree}"])).unwrap();
    assert_eq!(our_tree, real_tree);
}

#[test]
fn a_stale_patch_conflicts_instead_of_applying() {
    let (_scan, work_dir, layout, did) = seeded();
    let work = work_dir.path();

    std::fs::write(work.join("a.txt"), "original\n").unwrap();
    stage_and_commit(work, "base");
    let base = git(work, &["rev-parse", "HEAD"]);

    std::fs::write(work.join("a.txt"), "patched from original\n").unwrap();
    stage_and_commit(work, "feature");
    let patch = git(work, &["diff", &base, "HEAD"]);

    git(work, &["checkout", "-q", &base]);
    git(work, &["checkout", "-q", "-b", "drifted"]);
    std::fs::write(work.join("a.txt"), "diverged\n").unwrap();
    stage_and_commit(work, "drift");
    git(work, &["branch", "-q", "-f", "main", "HEAD"]);
    push_main(work, &layout, &did);

    let bare = layout.open(&did).unwrap();
    let tip = bare.find_ref(&main_ref()).unwrap().unwrap();
    let files = parse_patch(&patch).unwrap();
    let mut applier = PatchApplier::new(&bare, tip);
    match applier.step(&files).unwrap() {
        ApplyOutcome::Conflicted(conflicts) => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].path, "a.txt");
            assert_eq!(conflicts[0].reason, ConflictReason::DoesNotApply);
        }
        ApplyOutcome::Clean(_) => panic!("stale patch mustn't apply cleanly"),
    }
}

fn git_apply_applies(cwd: &Path, patch: &str) -> bool {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = knot_fixtures::command(cwd)
        .args(["apply", "--check"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("git is available");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(patch.as_bytes())
        .expect("write patch to git apply");
    child.wait().expect("git apply completes").success()
}

#[test]
fn apply_verdict_matches_git_apply() {
    let (_scan, work_dir, layout, did) = seeded();
    let work = work_dir.path();
    std::fs::write(work.join("a.txt"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
    stage_and_commit(work, "base");
    push_main(work, &layout, &did);

    let bare = layout.open(&did).unwrap();
    let tip = bare.find_ref(&main_ref()).unwrap().unwrap();

    let cases: [&str; 8] = [
        "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n",
        "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,3 @@\n nope\n-gone\n+HERE\n zero\n",
        "diff --git a/a.txt b/a.txt\nnew file mode 100644\n--- /dev/null\n+++ b/a.txt\n@@ -0,0 +1 @@\n+x\n",
        "diff --git a/ghost.txt b/ghost.txt\ndeleted file mode 100644\n--- a/ghost.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n",
        "diff --git a/fresh.txt b/fresh.txt\nnew file mode 100644\n--- /dev/null\n+++ b/fresh.txt\n@@ -0,0 +1 @@\n+brand new\n",
        "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -3,1 +3,1 @@\n-three\n+THREE\n",
        "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -5,1 +5,1 @@\n-five\n+FIVE\n",
        "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -2,1 +1,0 @@\n-two\n",
    ];

    cases.iter().for_each(|patch| {
        let git_clean = git_apply_applies(work, patch);
        let mut applier = PatchApplier::new(&bare, tip);
        let ours_clean = matches!(
            applier.step(&parse_patch(patch).unwrap()).unwrap(),
            ApplyOutcome::Clean(_)
        );
        assert_eq!(
            git_clean, ours_clean,
            "verdict disagrees with git apply for patch:\n{patch}"
        );
    });
}

fn git_apply_to_worktree_fails(cwd: &Path, patch: &str) -> bool {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = knot_fixtures::command(cwd)
        .args(["apply"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("git is available");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(patch.as_bytes())
        .expect("write patch to git apply");
    !child.wait().expect("git apply completes").success()
}

#[test]
fn refused_patches_conflict_with_the_reason_git_apply_rejects() {
    let (_scan, work_dir, layout, did) = seeded();
    let work = work_dir.path();
    std::fs::write(work.join("a.txt"), "present\n").unwrap();
    std::fs::write(work.join("dir"), "i am a file\n").unwrap();
    stage_and_commit(work, "base");
    push_main(work, &layout, &did);

    let bare = layout.open(&did).unwrap();
    let tip = bare.find_ref(&main_ref()).unwrap().unwrap();

    let create_existing = concat!(
        "diff --git a/a.txt b/a.txt\n",
        "new file mode 100644\n",
        "--- /dev/null\n",
        "+++ b/a.txt\n",
        "@@ -0,0 +1 @@\n",
        "+x\n",
    );
    let delete_missing = concat!(
        "diff --git a/ghost.txt b/ghost.txt\n",
        "deleted file mode 100644\n",
        "--- a/ghost.txt\n",
        "+++ /dev/null\n",
        "@@ -1 +0,0 @@\n",
        "-x\n",
    );
    let escape = concat!(
        "diff --git a/../escape.txt b/../escape.txt\n",
        "new file mode 100644\n",
        "--- /dev/null\n",
        "+++ b/../escape.txt\n",
        "@@ -0,0 +1 @@\n",
        "+boom\n",
    );
    let under_a_file = concat!(
        "diff --git a/dir/inner.txt b/dir/inner.txt\n",
        "new file mode 100644\n",
        "--- /dev/null\n",
        "+++ b/dir/inner.txt\n",
        "@@ -0,0 +1 @@\n",
        "+nested\n",
    );

    let cases: &[(&str, ConflictReason, &str)] = &[
        (
            create_existing,
            ConflictReason::AlreadyExists,
            "file already exists",
        ),
        (
            delete_missing,
            ConflictReason::DoesNotExist,
            "file doesn't exist",
        ),
        (escape, ConflictReason::DoesNotApply, "patch doesn't apply"),
        (
            under_a_file,
            ConflictReason::DoesNotApply,
            "patch doesn't apply",
        ),
    ];

    cases.iter().for_each(|(patch, reason, message)| {
        assert!(
            git_apply_to_worktree_fails(work, patch),
            "git apply must refuse:\n{patch}"
        );
        let mut applier = PatchApplier::new(&bare, tip);
        match applier.step(&parse_patch(patch).unwrap()).unwrap() {
            ApplyOutcome::Conflicted(conflicts) => {
                assert_eq!(conflicts[0].reason, *reason);
                assert_eq!(conflicts[0].reason.as_str(), *message);
            }
            ApplyOutcome::Clean(_) => panic!("must conflict, not apply cleanly:\n{patch}"),
        }
    });
}
