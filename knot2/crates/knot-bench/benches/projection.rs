use divan::Bencher;
use divan::counter::ItemsCount;
use knot_bench::{RepoCount, RosterCount, build_collaborator_roster, build_registry};

#[global_allocator]
static ALLOC: divan::AllocProfiler = divan::AllocProfiler::system();

const ROSTER_GRADES: &[u32] = &[64, 512, 4096];
const REGISTRY_GRADES: &[u64] = &[64, 4096];

fn main() {
    divan::main();
}

#[divan::bench(args = ROSTER_GRADES)]
fn collaborator_refresh(bencher: Bencher, collaborators: u32) {
    let built = build_collaborator_roster(RosterCount::new(collaborators));
    let index = built.index();
    index.rebuild().expect("rebuild");
    index
        .ensure_collaborators(built.repo())
        .expect("first fold");
    bencher
        .counter(ItemsCount::new(collaborators as usize))
        .bench_local(|| index.refresh_collaborators(built.repo()).expect("refresh"));
}

#[divan::bench(args = REGISTRY_GRADES)]
fn resolve(bencher: Bencher, repos: u64) {
    let registry = build_registry(RepoCount::new(repos));
    let index = registry.index();
    index.rebuild().expect("rebuild");
    let (owner, rkey) = registry.alias(repos / 2);
    bencher
        .counter(ItemsCount::new(1usize))
        .bench_local(|| index.resolve_repo(&owner, &rkey));
}
