use divan::Bencher;
use divan::counter::ItemsCount;
use knot_bench::{OpenLatency, RepoCount, build_registry, replay_boot};
use knot_types::RepoDid;

#[global_allocator]
static ALLOC: divan::AllocProfiler = divan::AllocProfiler::system();

const FABRIC_OPEN_MICROS: u64 = 200;

fn repo_counts() -> Vec<u64> {
    let Ok(raw) = std::env::var("KNOT_BENCH_REPOS") else {
        return vec![1, 1000];
    };
    let counts: Vec<u64> = raw
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            entry
                .parse::<u64>()
                .unwrap_or_else(|_| panic!("KNOT_BENCH_REPOS entry {entry:?} is not a repo count"))
        })
        .collect();
    assert!(
        !counts.is_empty(),
        "KNOT_BENCH_REPOS is set but lists no repo counts"
    );
    counts
}

fn main() {
    divan::main();
}

#[divan::bench(args = repo_counts())]
fn boot(bencher: Bencher, repos: u64) {
    let registry = build_registry(RepoCount::new(repos));
    bencher
        .counter(ItemsCount::new(registry.dids().len()))
        .bench_local(|| registry.index().rebuild().unwrap());
}

#[divan::bench(args = repo_counts())]
fn boot_fabric(bencher: Bencher, repos: u64) {
    let registry = build_registry(RepoCount::new(repos));
    let dids: Vec<RepoDid> = registry.dids().to_vec();
    bencher
        .counter(ItemsCount::new(dids.len()))
        .bench_local(|| {
            let index = registry.index();
            replay_boot(&index, &dids, OpenLatency::micros(FABRIC_OPEN_MICROS));
        });
}
