#![cfg(feature = "instrument")]

use knot_bench::{RepoCount, build_registry_checkpointed};
use knot_cob::instrument::measure;

#[test]
fn a_checkpointed_registry_write_reads_a_bounded_object_set() {
    let small = build_registry_checkpointed(RepoCount::new(512));
    let large = build_registry_checkpointed(RepoCount::new(1024));

    let (_, full_small) = measure(|| small.full_fold());
    let (_, full_large) = measure(|| large.full_fold());
    let (_, write_small) = measure(|| small.probe());
    let (_, write_large) = measure(|| large.probe());

    assert_eq!(
        full_small.get(),
        2 * 512,
        "full fold reads every change's commit and payload, so twice the change count"
    );
    assert_eq!(
        full_large.get(),
        2 * 1024,
        "full fold the checkpoint replaces is linear in repo count"
    );
    assert_eq!(
        write_small.get(),
        write_large.get(),
        "checkpointed write reads only the bounded suffix, the same object count at 512 \
         repos as at 1024"
    );
    assert!(
        write_large.get() <= 2 * 256,
        "per-write read set is bounded by twice the snapshot stride, not the repo count, \
         was {}",
        write_large.get()
    );
    assert!(
        write_large.get() < full_large.get() / 3,
        "checkpoint cuts per-write object reads far below the full fold, {} vs {}",
        write_large.get(),
        full_large.get()
    );
}
