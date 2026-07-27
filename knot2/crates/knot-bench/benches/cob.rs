use divan::Bencher;
use divan::counter::ItemsCount;
use knot_bench::{ChangeCount, RepoCount, build_linear_cob, build_registry_checkpointed};

#[global_allocator]
static ALLOC: divan::AllocProfiler = divan::AllocProfiler::system();

const GRADES: &[u32] = &[64, 512, 4096];
const REPO_GRADES: &[u64] = &[256, 1024, 4096];

fn main() {
    divan::main();
}

#[divan::bench(args = GRADES)]
fn fold(bencher: Bencher, changes: u32) {
    let cob = build_linear_cob(ChangeCount::new(changes));
    bencher
        .counter(ItemsCount::new(changes as usize))
        .bench_local(|| cob.fold());
}

#[divan::bench(args = REPO_GRADES)]
fn registry_write_checkpointed(bencher: Bencher, repos: u64) {
    let writer = build_registry_checkpointed(RepoCount::new(repos));
    bencher.bench_local(|| writer.probe());
}
