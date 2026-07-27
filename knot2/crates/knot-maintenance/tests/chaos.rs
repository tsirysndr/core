use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use knot_git::{
    EntryKind, Identity, Layout, NewCommit, RefUpdate, Repo, StagedAction, StagedChange,
};
use knot_maintenance::{
    GeometricFactor, ObjectCount, Options, PruneGrace, ReflogRetention, run_repo,
};
use knot_types::{AuthorName, BranchName, Email, Oid, RefName, RepoDid, UnixSeconds};

const DID: &str = "did:plc:squid";
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const BLOB_BYTES: usize = 8 * 1024 * 1024;
const HISTORY: u32 = 40;
const NOW_SECONDS: UnixSeconds = UnixSeconds::new(1_700_000_500);

fn incompressible(len: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state & 0xff) as u8
        })
        .collect()
}

fn identity() -> Identity {
    Identity {
        name: AuthorName::new("nel"),
        email: Email::new("nel@oyster.cafe"),
        time: UnixSeconds::new(1_700_000_000),
        offset_seconds: 0,
    }
}

fn options() -> Options {
    Options {
        repack_max_objects: ObjectCount::new(5_000_000),
        geometric_factor: GeometricFactor::full_repack(),
        prune_grace: PruneGrace::from_secs(0),
        reflog_floor: ReflogRetention::from_secs(i64::MAX as u64 / 4),
        commit_graph: true,
        multi_pack_index: true,
        bitmap: true,
    }
}

fn build_template(scan: &Path) {
    let layout = Layout::new(scan).with_default_branch(BranchName::new("main").unwrap());
    let did = RepoDid::new(DID).unwrap();
    let repo = layout.create(&did).unwrap();
    let main = RefName::new("refs/heads/main").unwrap();
    let empty = Oid::from_hex(EMPTY_TREE).unwrap();

    let big_tree = repo
        .write_staged_tree(
            empty,
            &[StagedChange {
                path: knot_types::RepoPath::new("big.bin").unwrap(),
                action: StagedAction::Put {
                    content: incompressible(BLOB_BYTES),
                    kind: EntryKind::Blob,
                },
            }],
        )
        .unwrap();
    let mut tip = repo
        .write_commit(&NewCommit {
            tree: big_tree,
            parents: Vec::new(),
            author: identity(),
            committer: identity(),
            message: "big".to_string(),
            extra_headers: Vec::new(),
        })
        .unwrap();
    (0..HISTORY).for_each(|index| {
        let tree = repo
            .write_staged_tree(
                empty,
                &[StagedChange {
                    path: knot_types::RepoPath::new(format!("file{index}.txt")).unwrap(),
                    action: StagedAction::Put {
                        content: format!("contents {index}").into_bytes(),
                        kind: EntryKind::Blob,
                    },
                }],
            )
            .unwrap();
        let next = repo
            .write_commit(&NewCommit {
                tree,
                parents: vec![tip],
                author: identity(),
                committer: identity(),
                message: format!("commit {index}"),
                extra_headers: Vec::new(),
            })
            .unwrap();
        let update = match index {
            0 => RefUpdate::Create {
                name: main.clone(),
                new: next,
            },
            _ => RefUpdate::Update {
                name: main.clone(),
                old: tip,
                new: next,
            },
        };
        repo.update_ref(&update).unwrap();
        tip = next;
    });
}

fn copy_tree(src: &Path, dst: &Path) {
    walkdir::WalkDir::new(src)
        .into_iter()
        .filter_map(Result::ok)
        .for_each(|entry| {
            let relative = entry.path().strip_prefix(src).unwrap();
            let target = dst.join(relative);
            if entry.file_type().is_dir() {
                std::fs::create_dir_all(&target).unwrap();
            } else {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::copy(entry.path(), &target).unwrap();
            }
        });
}

fn spawn_worker(scan: &Path) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "chaos_maintenance_worker", "--nocapture"])
        .env("KNOT_CHAOS_ROLE", "worker")
        .env("KNOT_CHAOS_SCAN", scan)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn chaos worker")
}

fn fsck_clean(bare: &Path) -> Result<(), String> {
    knot_fixtures::fsck(bare)
}

fn main_tip(scan: &Path) -> Option<Oid> {
    let layout = Layout::new(scan);
    let repo = layout
        .open(&RepoDid::new(DID).unwrap())
        .expect("repo must reopen cleanly after kill");
    repo.find_ref(&RefName::new("refs/heads/main").unwrap())
        .expect("references must be readable after kill")
}

#[test]
fn chaos_maintenance_worker() {
    if std::env::var("KNOT_CHAOS_ROLE").as_deref() != Ok("worker") {
        return;
    }
    let scan = std::env::var("KNOT_CHAOS_SCAN").unwrap();
    let layout = Layout::new(&scan);
    let repo = layout.open(&RepoDid::new(DID).unwrap()).unwrap();
    let _ = run_repo(&repo, NOW_SECONDS, &options());
}

#[test]
fn kill9_during_maintenance_leaves_a_consistent_repo() {
    let scratch = tempfile::tempdir().unwrap();
    let template = scratch.path().join("template");
    build_template(&template);
    let did = RepoDid::new(DID).unwrap();
    let tip = main_tip(&template).expect("template has a main tip");

    let warm = scratch.path().join("warm");
    copy_tree(&template, &warm);
    let started = Instant::now();
    let mut child = spawn_worker(&warm);
    child.wait().unwrap();
    let full = started.elapsed();
    let warm_repo = Repo::open(Layout::new(&warm).repo_path(&did).unwrap()).unwrap();
    assert!(
        warm_repo
            .git()
            .git_dir()
            .join("objects/info/commit-graph")
            .exists(),
        "uninterrupted maintenance run writes commit-graph"
    );

    let fractions = [0.20, 0.35, 0.45, 0.55, 0.65, 0.75, 0.85, 0.95, 1.10];
    let delays: Vec<Duration> = std::iter::once(Duration::from_millis(1))
        .chain(std::iter::once(Duration::from_millis(3)))
        .chain(fractions.iter().map(|fraction| full.mul_f64(*fraction)))
        .chain(std::iter::once(full.mul_f64(2.0)))
        .collect();

    delays.iter().enumerate().for_each(|(trial, delay)| {
        let scan = scratch.path().join(format!("scan-{trial}"));
        copy_tree(&template, &scan);
        let mut child = spawn_worker(&scan);
        std::thread::sleep(*delay);
        let _ = child.kill();
        child.wait().unwrap();

        let bare = Layout::new(&scan).repo_path(&did).unwrap();
        fsck_clean(&bare).unwrap_or_else(|errors| {
            panic!("trial {trial}: killed maintenance run left corrupt repo:\n{errors}")
        });
        assert_eq!(
            main_tip(&scan),
            Some(tip),
            "trial {trial}: maintenance never changes branch value, so main must still resolve to tip"
        );

        let recovered = Repo::open(&bare).unwrap();
        run_repo(&recovered, NOW_SECONDS, &options()).unwrap_or_else(|error| {
            panic!("trial {trial}: maintenance must self-heal after crash, got {error}")
        });
        fsck_clean(&bare).unwrap_or_else(|errors| {
            panic!("trial {trial}: self-heal pass left corrupt repo:\n{errors}")
        });
        assert_eq!(
            main_tip(&scan),
            Some(tip),
            "trial {trial}: recovered repo still resolves main to tip"
        );
    });
}
