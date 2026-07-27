use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use knot_git::Layout;
use knot_types::{Oid, RefName, RepoDid};

mod common;
use common::{must, pack_objects, receive_request};

const DID: &str = "did:plc:squid";
const BLOB_BYTES: usize = 32 * 1024 * 1024;

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

struct Seed {
    c1: String,
    c2: String,
    request: std::path::PathBuf,
}

fn build_seed(scratch: &Path) -> Seed {
    let work = scratch.join("work");
    std::fs::create_dir_all(&work).unwrap();
    must(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("base.txt"), "baseline\n").unwrap();
    must(&work, &["add", "-A"]);
    must(&work, &["commit", "-q", "-m", "c1"]);
    let c1 = must(&work, &["rev-parse", "HEAD"]);
    std::fs::write(work.join("big.bin"), incompressible(BLOB_BYTES)).unwrap();
    must(&work, &["add", "-A"]);
    must(&work, &["commit", "-q", "-m", "c2"]);
    let c2 = must(&work, &["rev-parse", "HEAD"]);

    let oids: Vec<String> = must(&work, &["rev-list", "--objects", &c2, "--not", &c1])
        .lines()
        .map(|line| line.split_whitespace().next().unwrap().to_string())
        .collect();
    let pack = pack_objects(&work, &oids);
    let request = scratch.join("c2.request");
    std::fs::write(
        &request,
        receive_request("refs/heads/main", &c1, &c2, &pack),
    )
    .unwrap();
    Seed { c1, c2, request }
}

fn fresh_repo(scratch: &Path, trial: usize, seed: &Seed) -> std::path::PathBuf {
    let scan = scratch.join(format!("scan-{trial}"));
    let layout = Layout::new(&scan);
    let did = RepoDid::new(DID).unwrap();
    layout.create(&did).unwrap();
    let bare = layout.repo_path(&did).unwrap();
    must(
        scratch.join("work").as_path(),
        &[
            "push",
            "-q",
            bare.to_str().unwrap(),
            &format!("{}:refs/heads/main", seed.c1),
        ],
    );
    scan
}

fn spawn_worker(scan: &Path, request: &Path) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "chaos_receive_worker", "--nocapture"])
        .env("KNOT_CHAOS_ROLE", "worker")
        .env("KNOT_CHAOS_SCAN", scan)
        .env("KNOT_CHAOS_REQUEST", request)
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
        .expect("repo must reopen cleanly after a kill");
    repo.find_ref(&RefName::new("refs/heads/main").unwrap())
        .expect("references must be readable after a kill")
}

#[test]
fn chaos_receive_worker() {
    if std::env::var("KNOT_CHAOS_ROLE").as_deref() != Ok("worker") {
        return;
    }
    let scan = std::env::var("KNOT_CHAOS_SCAN").unwrap();
    let request = std::env::var("KNOT_CHAOS_REQUEST").unwrap();
    let layout = Layout::new(&scan);
    let repo = layout.open(&RepoDid::new(DID).unwrap()).unwrap();
    let body = std::fs::read(&request).unwrap();
    let _ = knot_pack::receive_pack(&repo, &body);
}

#[test]
fn kill9_during_receive_pack_leaves_a_consistent_repo() {
    let scratch = tempfile::tempdir().unwrap();
    let seed = build_seed(scratch.path());
    let c1 = Oid::from_hex(&seed.c1).unwrap();
    let c2 = Oid::from_hex(&seed.c2).unwrap();
    let did = RepoDid::new(DID).unwrap();

    let warm_scan = fresh_repo(scratch.path(), 9000, &seed);
    let started = Instant::now();
    let mut warm = spawn_worker(&warm_scan, &seed.request);
    warm.wait().unwrap();
    let full = started.elapsed();
    assert_eq!(
        main_tip(&warm_scan),
        Some(c2),
        "uninterrupted receive must fast-forward main to new tip"
    );

    let fractions = [0.30, 0.45, 0.55, 0.62, 0.70, 0.78, 0.85, 0.92, 1.05, 1.25];
    let delays: Vec<Duration> = std::iter::once(Duration::from_millis(2))
        .chain(std::iter::once(Duration::from_millis(5)))
        .chain(fractions.iter().map(|fraction| full.mul_f64(*fraction)))
        .chain(std::iter::once(full.mul_f64(2.0)))
        .chain(std::iter::once(full.mul_f64(2.0)))
        .collect();

    let outcomes: Vec<Oid> = delays
        .iter()
        .enumerate()
        .map(|(trial, delay)| {
            let scan = fresh_repo(scratch.path(), trial, &seed);
            let mut child = spawn_worker(&scan, &seed.request);
            std::thread::sleep(*delay);
            let _ = child.kill();
            child.wait().unwrap();

            let bare = Layout::new(&scan).repo_path(&did).unwrap();
            fsck_clean(&bare).unwrap_or_else(|errors| {
                panic!("trial {trial}: killed receive left a corrupt repo:\n{errors}")
            });
            let tip = main_tip(&scan).unwrap_or_else(|| {
                panic!("trial {trial}: main vanished after a kill, acknowledged ref was lost")
            });
            assert!(
                tip == c1 || tip == c2,
                "trial {trial}: main must hold either acknowledged baseline or completed tip, never a torn value, got {tip}"
            );
            tip
        })
        .collect();

    assert!(
        outcomes.contains(&c1),
        "no trial was interrupted before ref update; chaos window never opened"
    );
    assert!(
        outcomes.contains(&c2),
        "no trial ran to completion; receive never finished under chosen delays"
    );
}
