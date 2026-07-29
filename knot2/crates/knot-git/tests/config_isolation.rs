use std::path::Path;
use std::sync::atomic::AtomicBool;

use knot_git::{ArchiveFormat, ArchiveLimit};
use knot_types::Oid;

mod common;
use common::{commit_file, contains, git_ok as git, seeded};

fn archive_without_isolation(bare_path: &Path, head: Oid) -> Vec<u8> {
    let permissive = gix::open_opts(bare_path, gix::open::Options::default()).unwrap();
    let tree = permissive
        .find_object(head.object_id())
        .unwrap()
        .peel_to_tree()
        .unwrap();
    let (stream, _index) = permissive.worktree_stream(tree.id).unwrap();
    let mut out = std::io::Cursor::new(Vec::new());
    permissive
        .worktree_archive(
            stream,
            &mut out,
            gix::progress::Discard,
            &AtomicBool::new(false),
            gix_archive::Options {
                format: gix_archive::Format::Tar,
                tree_prefix: None,
                modification_time: 0,
            },
        )
        .unwrap();
    out.into_inner()
}

#[test]
fn a_filter_driver_pulled_in_by_an_include_never_runs_for_a_served_archive() {
    let (_scan, work_dir, layout, did) = seeded();
    let work = work_dir.path();
    let bare_path = layout.repo_path(&did).unwrap();
    std::fs::write(work.join(".gitattributes"), "payload.txt filter=knotpwn\n").unwrap();
    commit_file(work, "payload.txt", "kelp\n", "seed");
    git(work, &["push", "-q", bare_path.to_str().unwrap(), "main"]);
    let head = Oid::from_hex(&git(work, &["rev-parse", "HEAD"])).unwrap();

    let ambient = tempfile::tempdir().unwrap();
    let driver = ambient.path().join("driver.cfg");
    std::fs::write(
        &driver,
        "[filter \"knotpwn\"]\n\tsmudge = sed s/kelp/pwned/\n\trequired = true\n",
    )
    .unwrap();
    let config_path = bare_path.join("config");
    let local = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(
        &config_path,
        format!("{local}[include]\n\tpath = {}\n", driver.display()),
    )
    .unwrap();

    let via_git = knot_fixtures::command(&bare_path)
        .args(["archive", "--format=tar", "main"])
        .output()
        .unwrap();
    assert!(
        via_git.status.success(),
        "git archive failed:\n{}",
        String::from_utf8_lossy(&via_git.stderr)
    );
    assert!(
        contains(&via_git.stdout, b"pwned"),
        "git archived with the include in place and its output has no pwned in it, so the driver \
         never ran"
    );
    assert!(
        contains(&archive_without_isolation(&bare_path, head), b"pwned"),
        "gix archived the same repo under default permissions and its output has no pwned in it, \
         so worktree_stream no longer applies filters"
    );

    let bare = layout.open(&did).unwrap();
    let tree = bare.peel_to_tree(head).unwrap();
    let mut out = std::io::Cursor::new(Vec::new());
    bare.write_archive(
        tree,
        ArchiveFormat::Tar,
        None,
        ArchiveLimit::new(u64::MAX),
        &mut out,
    )
    .unwrap();
    let served = out.into_inner();

    assert!(
        std::fs::read_to_string(&config_path)
            .unwrap()
            .contains("[include]"),
        "opening the repo rewrote its config and removed the include, so gix never read the \
         driver definition for the archive below"
    );
    assert!(
        contains(&served, b"kelp"),
        "the served archive contains the blob as it was pushed"
    );
    assert!(
        !contains(&served, b"pwned"),
        "the knot ran a filter driver defined by config outside the repository"
    );
}

#[test]
fn a_pushed_replace_ref_never_substitutes_an_object_the_knot_reads() {
    let (_scan, work_dir, layout, did) = seeded();
    let work = work_dir.path();
    let bare_path = layout.repo_path(&did).unwrap();

    commit_file(work, "payload.txt", "kelp\n", "seed");
    git(work, &["push", "-q", bare_path.to_str().unwrap(), "main"]);
    let original = Oid::from_hex(&git(work, &["rev-parse", "HEAD:payload.txt"])).unwrap();
    commit_file(work, "payload.txt", "pwned\n", "second");
    git(work, &["push", "-q", bare_path.to_str().unwrap(), "main"]);
    let substitute = Oid::from_hex(&git(work, &["rev-parse", "HEAD:payload.txt"])).unwrap();

    git(
        &bare_path,
        &[
            "update-ref",
            &format!("refs/replace/{}", original.to_hex()),
            &substitute.to_hex(),
        ],
    );

    assert_eq!(
        git(&bare_path, &["cat-file", "blob", &original.to_hex()]),
        "pwned",
        "git read the replaced object as itself, so this fixture never armed the substitution"
    );
    assert_eq!(
        layout.open(&did).unwrap().read_blob(original).unwrap(),
        b"kelp\n",
        "a pushed replace ref rewrote what the knot serves for an object"
    );
}
