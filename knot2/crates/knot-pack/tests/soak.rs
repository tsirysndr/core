use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::Duration;

use axum::Router;
use knot_bench::{ChurnCount, CommitCount, HistorySpec, PathCount, write_history};
use knot_git::Layout;
use knot_pack::{RepoLookup, RepoResolver, RepoTarget};
use knot_types::RepoDid;

mod common;
use common::must;

fn serve_dids() -> std::sync::Arc<dyn RepoResolver> {
    std::sync::Arc::new(|target: &RepoTarget| match target {
        RepoTarget::Did(did) => RepoLookup::Hosted(did.clone()),
        RepoTarget::OwnerPath(_, _) => RepoLookup::Unhosted,
    })
}

const BLOB_BYTES: usize = 16 * 1024 * 1024;
const CONCURRENCY: usize = 10;
const ROUNDS: usize = 5;
const PAGE_BYTES: u64 = 4096;
const OOM_CEILING: u64 = 1024 * 1024 * 1024;
const GROWTH_SLACK: u64 = 64 * 1024 * 1024;
const CURVE_LEVELS: [usize; 5] = [1, 2, 4, 8, 16];
const PER_CONNECTION_CEILING: u64 = 96 * 1024 * 1024;
const SUBLINEAR_SLACK: u64 = 32 * 1024 * 1024;

static RSS_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn incompressible(len: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state & 0xff) as u8
        })
        .collect()
}

async fn spawn(router: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

fn rss_bytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").expect("/proc/self/statm is readable");
    statm
        .split_whitespace()
        .nth(1)
        .and_then(|pages| pages.parse::<u64>().ok())
        .map(|pages| pages * PAGE_BYTES)
        .expect("statm lists the resident page count")
}

fn clone_child(remote: &str, dest: &Path) -> Child {
    knot_fixtures::command(dest.parent().unwrap_or(dest))
        .args(["clone", "--bare", "--quiet", remote, dest.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git clone spawns")
}

struct Soak {
    _scan: tempfile::TempDir,
    scratch: tempfile::TempDir,
    remote: String,
    tip: String,
}

async fn serve_large_repo() -> Soak {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    layout.create(&did).unwrap();
    let bare = layout.repo_path(&did).unwrap();

    let scratch = tempfile::tempdir().unwrap();
    let work = scratch.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    must(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("big.bin"), incompressible(BLOB_BYTES)).unwrap();
    must(&work, &["add", "-A"]);
    must(&work, &["commit", "-q", "-m", "large"]);
    must(&work, &["push", "-q", bare.to_str().unwrap(), "main"]);
    must(bare.as_path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let tip = must(&work, &["rev-parse", "HEAD"]);

    let addr = spawn(knot_pack::router(
        layout,
        serve_dids(),
        std::sync::Arc::new(knot_runtime::SystemClock),
    ))
    .await;
    let remote = format!("http://{addr}/{}", did.as_str());

    Soak {
        _scan: scan,
        scratch,
        remote,
        tip,
    }
}

async fn serve_wide_history() -> Soak {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let repo = layout.create(&did).unwrap();
    let tip = write_history(
        &repo,
        HistorySpec {
            commits: CommitCount::new(256),
            paths: PathCount::new(4096),
            churn: ChurnCount::new(16),
        },
    )
    .to_hex();

    let scratch = tempfile::tempdir().unwrap();
    let addr = spawn(knot_pack::router(
        layout,
        serve_dids(),
        std::sync::Arc::new(knot_runtime::SystemClock),
    ))
    .await;
    let remote = format!("http://{addr}/{}", did.as_str());

    Soak {
        _scan: scan,
        scratch,
        remote,
        tip,
    }
}

fn drain_storm(remote: &str, scratch: &Path, round: usize, concurrency: usize, tip: &str) -> u64 {
    let dests: Vec<std::path::PathBuf> = (0..concurrency)
        .map(|index| scratch.join(format!("clone-{round}-{index}")))
        .collect();
    let mut children: Vec<Child> = dests.iter().map(|dest| clone_child(remote, dest)).collect();

    let mut peak = rss_bytes();
    let mut pending = true;
    while pending {
        peak = peak.max(rss_bytes());
        std::thread::sleep(Duration::from_millis(3));
        pending = children
            .iter_mut()
            .any(|child| matches!(child.try_wait(), Ok(None)));
    }

    children.into_iter().enumerate().for_each(|(index, child)| {
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "round {round} clone {index} failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    });

    dests.iter().for_each(|dest| {
        assert_eq!(
            must(dest, &["rev-parse", "HEAD"]),
            tip,
            "soak clone must reproduce repo tip"
        );
        must(dest, &["fsck", "--connectivity-only", "--no-progress"]);
        std::fs::remove_dir_all(dest).unwrap();
    });

    peak.max(rss_bytes())
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_clone_memory_cost_curve() {
    let _rss_gate = RSS_GATE.lock().await;
    let soak = serve_wide_history().await;

    let baseline = rss_bytes();
    let mut peak = baseline;
    let curve: Vec<(usize, u64)> = CURVE_LEVELS
        .into_iter()
        .map(|concurrency| {
            peak = peak.max(drain_storm(
                &soak.remote,
                soak.scratch.path(),
                concurrency,
                concurrency,
                &soak.tip,
            ));
            (concurrency, peak.saturating_sub(baseline))
        })
        .collect();

    curve.iter().for_each(|(concurrency, delta)| {
        let per_connection = delta / *concurrency as u64;
        println!(
            "{concurrency} concurrent clones: cumulative +{} MiB, ~{} MiB per connection",
            delta / (1024 * 1024),
            per_connection / (1024 * 1024)
        );
        assert!(
            per_connection <= PER_CONNECTION_CEILING,
            "per-connection high-water for {concurrency} clones is {} MiB, past {} MiB ceiling",
            per_connection / (1024 * 1024),
            PER_CONNECTION_CEILING / (1024 * 1024)
        );
    });

    let (_, single) = curve[0];
    let (top, top_delta) = *curve.last().unwrap();
    let top_per_connection = top_delta / top as u64;
    assert!(
        top_per_connection <= single + SUBLINEAR_SLACK,
        "memory grows super-linearly with concurrency. {top} clones cost {} MiB per connection \
         against {} MiB for single clone, past the {} MiB slack. Pack is shared, so the \
         per-connection high-water must stay flat as connections rise",
        top_per_connection / (1024 * 1024),
        single / (1024 * 1024),
        SUBLINEAR_SLACK / (1024 * 1024)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_clones_of_a_large_repo_stay_bounded() {
    let _rss_gate = RSS_GATE.lock().await;
    let soak = serve_large_repo().await;
    let scratch = &soak.scratch;
    let remote = soak.remote.clone();
    let tip = soak.tip.clone();

    let baseline = rss_bytes();
    let mut peak = baseline;
    let after_round: Vec<u64> = (0..ROUNDS)
        .map(|round| {
            peak = peak.max(drain_storm(
                &remote,
                scratch.path(),
                round,
                CONCURRENCY,
                &tip,
            ));
            rss_bytes()
        })
        .collect();

    assert!(
        peak < OOM_CEILING,
        "serving {CONCURRENCY} concurrent clones mustn't balloon resident memory: peak {} MiB exceeds {} MiB ceiling",
        peak / (1024 * 1024),
        OOM_CEILING / (1024 * 1024)
    );

    let settled = after_round[..ROUNDS - 1].iter().copied().max().unwrap();
    let last = after_round[ROUNDS - 1];
    assert!(
        last <= settled + GROWTH_SLACK,
        "resident memory is still climbing at final round, a leak: settled at {} MiB, round {ROUNDS} left {} MiB",
        settled / (1024 * 1024),
        last / (1024 * 1024)
    );
}
