use divan::Bencher;
use divan::counter::ItemsCount;
use knot_bench::{RefCount, build_many_refs};

#[global_allocator]
static ALLOC: divan::AllocProfiler = divan::AllocProfiler::system();

const GRADES: &[u32] = &[16, 256, 4096];

fn main() {
    divan::main();
}

#[divan::bench(args = GRADES)]
fn advert_uncached(bencher: Bencher, refs: u32) {
    let built = build_many_refs(RefCount::new(refs));
    let count = built.repo().references().unwrap().len();
    bencher
        .counter(ItemsCount::new(count))
        .bench_local(|| built.repo().references().unwrap());
}

#[divan::bench(args = GRADES)]
fn advert_cached(bencher: Bencher, refs: u32) {
    let built = build_many_refs(RefCount::new(refs));
    let count = built.repo().advertised_refs().unwrap().len();
    bencher
        .counter(ItemsCount::new(count))
        .bench_local(|| built.repo().advertised_refs().unwrap());
}
