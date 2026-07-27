mod common;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use common::{backdate, incompressible, object_path, oid_of, pointer_blob};
use knot_git::{EntryKind, Identity, Layout, NewCommit, RefUpdate, StagedAction, StagedChange};
use knot_lfs::{ClaimedSize, DiskStore, LfsOid, LfsSize, LfsStore, LfsStorePath, collect_repo};
use knot_types::{AuthorName, BranchName, Email, Oid, RefName, RepoDid, UnixSeconds};

const DID: &str = "did:plc:squid";
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const PUT_BYTES: usize = 48 * 1024 * 1024;
const REFERENCED: usize = 24;
const UNREFERENCED: usize = 320;
const GRACE: Duration = Duration::from_secs(86_400);
const BACKDATE: Duration = Duration::from_secs(60 * 86_400);

fn did() -> RepoDid {
    RepoDid::new(DID).unwrap()
}

fn spawn_worker(role: &str, envs: &[(&str, &Path)]) -> std::process::Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", &format!("chaos_{role}_worker"), "--nocapture"])
        .env("KNOT_CHAOS_ROLE", role)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    envs.iter().for_each(|(key, value)| {
        command.env(key, value);
    });
    command.spawn().expect("spawn chaos worker")
}

fn kill_after(mut child: std::process::Child, delay: Duration) {
    std::thread::sleep(delay);
    let _ = child.kill();
    child.wait().unwrap();
}

fn delays(full: Duration) -> Vec<Duration> {
    let fractions = [0.20, 0.35, 0.45, 0.55, 0.65, 0.75, 0.85, 0.95];
    std::iter::once(Duration::from_millis(1))
        .chain(std::iter::once(Duration::from_millis(3)))
        .chain(fractions.iter().map(|fraction| full.mul_f64(*fraction)))
        .chain(std::iter::once(full.mul_f64(2.0)))
        .collect()
}

#[test]
fn chaos_put_worker() {
    if std::env::var("KNOT_CHAOS_ROLE").as_deref() != Ok("put") {
        return;
    }
    let store_dir = std::env::var("KNOT_CHAOS_STORE").unwrap();
    let store = DiskStore::open(LfsStorePath::new(&store_dir)).unwrap();
    let body = incompressible(PUT_BYTES, 0x2545_f491_4f6c_dd1d);
    let oid = oid_of(&body);
    let _ = store.put(
        &did(),
        &oid,
        ClaimedSize::new(body.len() as u64),
        &mut &body[..],
    );
}

#[test]
fn kill9_during_put_object_never_leaves_a_torn_object() {
    let body = incompressible(PUT_BYTES, 0x2545_f491_4f6c_dd1d);
    let oid = oid_of(&body);
    let scratch = tempfile::tempdir().unwrap();

    let warm = scratch.path().join("warm");
    std::fs::create_dir_all(&warm).unwrap();
    let started = Instant::now();
    spawn_worker("put", &[("KNOT_CHAOS_STORE", &warm)])
        .wait()
        .unwrap();
    let full = started.elapsed();
    assert!(
        object_path(&warm, &oid).is_file(),
        "an uninterrupted put stores the object"
    );

    let landed: Vec<bool> = delays(full)
        .iter()
        .enumerate()
        .map(|(trial, delay)| {
            let store_dir = scratch.path().join(format!("store-{trial}"));
            std::fs::create_dir_all(&store_dir).unwrap();
            kill_after(
                spawn_worker("put", &[("KNOT_CHAOS_STORE", &store_dir)]),
                *delay,
            );

            let final_path = object_path(&store_dir, &oid);
            let present = final_path.is_file();
            if present {
                let bytes = std::fs::read(&final_path).unwrap();
                assert_eq!(
                    bytes.len(),
                    PUT_BYTES,
                    "trial {trial}: a visible object is never truncated"
                );
                assert_eq!(
                    oid_of(&bytes),
                    oid,
                    "trial {trial}: a visible object always hashes to its oid"
                );
            }

            let store = DiskStore::open(LfsStorePath::new(&store_dir)).unwrap();
            let incoming: Vec<_> = std::fs::read_dir(store_dir.join(".incoming"))
                .unwrap()
                .collect();
            assert!(
                incoming.is_empty(),
                "trial {trial}: boot sweep clears abandoned uploads, found {incoming:?}"
            );
            assert_eq!(
                store.probe(&did(), &oid).unwrap().is_some(),
                present,
                "trial {trial}: the boot sweep never deletes a stored object"
            );
            present
        })
        .collect();

    assert!(
        landed.iter().any(|present| !present),
        "some trial must be killed before the rename, or the kill delays are all too long"
    );
    assert!(
        landed.iter().any(|present| *present),
        "some trial must complete, or the kill delays are all too short"
    );
}

fn identity() -> Identity {
    Identity {
        name: AuthorName::new("nel"),
        email: Email::new("nel@oyster.cafe"),
        time: UnixSeconds::new(1_700_000_000),
        offset_seconds: 0,
    }
}

struct GcFixture {
    referenced: Vec<(LfsOid, Vec<u8>)>,
    unreferenced: Vec<LfsOid>,
}

fn build_gc_fixture(scan: &Path, store_dir: &Path) -> GcFixture {
    let layout = Layout::new(scan).with_default_branch(BranchName::new("main").unwrap());
    let repo = layout.create(&did()).unwrap();
    let store = DiskStore::open(LfsStorePath::new(store_dir)).unwrap();

    let referenced: Vec<(LfsOid, Vec<u8>)> = (0..REFERENCED)
        .map(|index| {
            let body = incompressible(2048, 0x9e37_79b9_7f4a_7c15 ^ index as u64);
            let oid = oid_of(&body);
            store
                .put(
                    &did(),
                    &oid,
                    ClaimedSize::new(body.len() as u64),
                    &mut &body[..],
                )
                .unwrap();
            backdate(&object_path(store_dir, &oid), BACKDATE);
            (oid, body)
        })
        .collect();

    let unreferenced: Vec<LfsOid> = (0..UNREFERENCED)
        .map(|index| {
            let body = incompressible(512, 0xdead_beef_cafe_f00d ^ index as u64);
            let oid = oid_of(&body);
            store
                .put(
                    &did(),
                    &oid,
                    ClaimedSize::new(body.len() as u64),
                    &mut &body[..],
                )
                .unwrap();
            backdate(&object_path(store_dir, &oid), BACKDATE);
            oid
        })
        .collect();

    let changes: Vec<StagedChange> = referenced
        .iter()
        .enumerate()
        .map(|(index, (oid, body))| StagedChange {
            path: knot_types::RepoPath::new(format!("media/clip{index}.bin")).unwrap(),
            action: StagedAction::Put {
                content: pointer_blob(oid, LfsSize::new(body.len() as u64)),
                kind: EntryKind::Blob,
            },
        })
        .collect();
    let empty = Oid::from_hex(EMPTY_TREE).unwrap();
    let tree = repo.write_staged_tree(empty, &changes).unwrap();
    let tip = repo
        .write_commit(&NewCommit {
            tree,
            parents: Vec::new(),
            author: identity(),
            committer: identity(),
            message: "add media".to_string(),
            extra_headers: Vec::new(),
        })
        .unwrap();
    repo.update_ref(&RefUpdate::Create {
        name: RefName::new("refs/heads/main").unwrap(),
        new: tip,
    })
    .unwrap();

    GcFixture {
        referenced,
        unreferenced,
    }
}

fn run_gc(scan: &Path, store_dir: &Path) {
    let layout = Layout::new(scan);
    let repo = layout.open(&did()).unwrap();
    let store = DiskStore::open(LfsStorePath::new(store_dir)).unwrap();
    let _ = collect_repo(&store, &repo, &did(), GRACE, SystemTime::now());
}

#[test]
fn chaos_gc_worker() {
    if std::env::var("KNOT_CHAOS_ROLE").as_deref() != Ok("gc") {
        return;
    }
    let scan = std::env::var("KNOT_CHAOS_SCAN").unwrap();
    let store_dir = std::env::var("KNOT_CHAOS_STORE").unwrap();
    run_gc(Path::new(&scan), Path::new(&store_dir));
}

#[test]
fn kill9_during_gc_never_loses_a_referenced_object() {
    let scratch = tempfile::tempdir().unwrap();

    let warm_scan = scratch.path().join("warm-scan");
    let warm_store = scratch.path().join("warm-store");
    let warm_fixture = build_gc_fixture(&warm_scan, &warm_store);
    let started = Instant::now();
    spawn_worker(
        "gc",
        &[
            ("KNOT_CHAOS_SCAN", &warm_scan),
            ("KNOT_CHAOS_STORE", &warm_store),
        ],
    )
    .wait()
    .unwrap();
    let full = started.elapsed();
    let warm_disk = DiskStore::open(LfsStorePath::new(&warm_store)).unwrap();
    warm_fixture.referenced.iter().for_each(|(oid, _)| {
        assert!(
            warm_disk.probe(&did(), oid).unwrap().is_some(),
            "an uninterrupted gc keeps every referenced object"
        );
    });
    warm_fixture.unreferenced.iter().for_each(|oid| {
        assert_eq!(
            warm_disk.probe(&did(), oid).unwrap(),
            None,
            "an uninterrupted gc reclaims every expired orphan"
        );
    });

    delays(full).iter().enumerate().for_each(|(trial, delay)| {
        let scan = scratch.path().join(format!("scan-{trial}"));
        let store_dir = scratch.path().join(format!("store-{trial}"));
        let fixture = build_gc_fixture(&scan, &store_dir);
        kill_after(
            spawn_worker(
                "gc",
                &[("KNOT_CHAOS_SCAN", &scan), ("KNOT_CHAOS_STORE", &store_dir)],
            ),
            *delay,
        );

        let store = DiskStore::open(LfsStorePath::new(&store_dir)).unwrap();
        fixture.referenced.iter().for_each(|(oid, body)| {
            assert_eq!(
                store.probe(&did(), oid).unwrap(),
                Some(LfsSize::new(body.len() as u64)),
                "trial {trial}: a referenced object remains stored after a killed sweep"
            );
        });

        run_gc(&scan, &store_dir);
        fixture.referenced.iter().for_each(|(oid, _)| {
            assert!(
                store.probe(&did(), oid).unwrap().is_some(),
                "trial {trial}: a referenced object remains stored after the self-heal pass"
            );
        });
        fixture.unreferenced.iter().for_each(|oid| {
            assert_eq!(
                store.probe(&did(), oid).unwrap(),
                None,
                "trial {trial}: the self-heal pass finishes the interrupted reclaim"
            );
        });
    });
}
