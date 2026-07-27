use std::collections::BTreeSet;

use futures::future::join_all;
use futures::stream::StreamExt;
use knot_sim::{Outcome, Step, Trace};

fn status_of(step: &Step) -> Option<u16> {
    match step.outcome {
        Outcome::Answered { status, .. } => Some(status.get()),
        Outcome::Killed => None,
    }
}

fn union_has(traces: &[Trace], predicate: impl Fn(&Step) -> bool) -> bool {
    traces
        .iter()
        .any(|trace| trace.steps.iter().any(&predicate))
}

const READ_OPS: [&str; 11] = [
    "version",
    "owner",
    "listMembers",
    "didJson",
    "infoRefs",
    "branches",
    "log",
    "describeRepo",
    "tree",
    "blob",
    "languages",
];

fn every_step_with<'a>(
    traces: &'a [Trace],
    fault: &'a str,
    holds: impl Fn(&Step) -> bool + 'a,
) -> (bool, usize) {
    let matching: Vec<&Step> = traces
        .iter()
        .flat_map(|trace| trace.steps.iter())
        .filter(|step| step.fault == fault)
        .collect();
    (matching.iter().all(|step| holds(step)), matching.len())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_same_seed_produces_an_identical_whole_system_trace() {
    futures::stream::iter([1u64, 7, 42, 100, 2026])
        .for_each(|seed| async move {
            let first = knot_sim::run(seed, 16).await;
            let second = knot_sim::run(seed, 16).await;
            assert_eq!(
                first, second,
                "seed {seed} must replay to an identical trace"
            );
            assert_eq!(
                first.digest(),
                second.digest(),
                "seed {seed} digest is stable"
            );
        })
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_execution_stays_stable_across_fifty_repeats() {
    let baseline = knot_sim::run(7, 18).await.digest();
    let digests: Vec<u64> = futures::stream::iter(0..50)
        .then(|_| knot_sim::run(7, 18))
        .map(|trace| trace.digest())
        .collect()
        .await;
    assert!(
        digests.iter().all(|digest| *digest == baseline),
        "4-thread schedule leaked into trace: {} of 50 replays diverged",
        digests.iter().filter(|digest| **digest != baseline).count()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn distinct_seeds_produce_distinct_traces() {
    let digests: Vec<u64> = join_all((0u64..8).map(|seed| knot_sim::run(seed, 16)))
        .await
        .iter()
        .map(Trace::digest)
        .collect();
    let unique: BTreeSet<u64> = digests.iter().copied().collect();
    assert_eq!(
        unique.len(),
        digests.len(),
        "every seed must drive different whole-system trace: {digests:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_round_count_extends_the_same_prefix() {
    let short = knot_sim::run(7, 10).await;
    let long = knot_sim::run(7, 20).await;
    assert_ne!(short.digest(), long.digest());
    assert_eq!(
        short.steps,
        long.steps[..short.steps.len()],
        "longer run extends the same prefix the shorter run produced"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_injected_failures_are_correlated_with_their_observable_outcomes() {
    let traces = join_all([1u64, 7, 13, 42, 99, 2026].map(|seed| knot_sim::run(seed, 16))).await;

    let (skew_all_401, skew_count) =
        every_step_with(&traces, "clock_skew", |step| status_of(step) == Some(401));
    assert!(skew_count > 0, "clock-skew fault must actually fire");
    assert!(
        skew_all_401,
        "every clock-skewed token must produce 401 expiry rejection, not just one example"
    );

    let (drop_all_503, drop_count) = every_step_with(&traces, "drop_identity", |step| {
        status_of(step) == Some(503)
    });
    assert!(drop_count > 0, "drop-identity fault must actually fire");
    assert!(
        drop_all_503,
        "every dropped identity resolution must surface as 503 upstream-unavailable failure"
    );

    let (killed_all_killed, killed_count) = every_step_with(&traces, "killed", |step| {
        matches!(step.outcome, Outcome::Killed) && READ_OPS.contains(&step.op)
    });
    assert!(killed_count > 0, "kill fault must actually fire");
    assert!(
        killed_all_killed,
        "killed connection must record a Killed outcome and is only injected on read paths"
    );

    assert!(
        union_has(&traces, |step| step.op == "probe"
            && step.fault == "none"
            && status_of(step) == Some(403)),
        "resolved stranger with no fault must be denied 403 by access-control layer"
    );

    [
        "addMember",
        "addCollaborator",
        "createRepo",
        "maintain",
        "describeRepo",
        "listMembers",
        "infoRefs",
        "didJson",
        "branches",
        "log",
        "tree",
        "blob",
        "languages",
    ]
    .iter()
    .for_each(|op| {
        assert!(
            union_has(&traces, |step| step.op == *op
                && status_of(step) == Some(200)),
            "{op} path must succeed against the doubles in at least one seed"
        );
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_final_projection_matches_an_independent_model() {
    let seeds = [1u64, 7, 13, 42, 99, 2026];
    let predicted: Vec<knot_sim::Projection> = seeds
        .iter()
        .map(|seed| knot_sim::predict(*seed, 18))
        .collect();
    let runs = join_all(seeds.map(|seed| knot_sim::run(seed, 18))).await;

    seeds
        .iter()
        .zip(predicted.iter())
        .zip(runs.iter())
        .for_each(|((seed, model), trace)| {
            let last = trace.snapshots.last().expect("at least one round");
            assert_eq!(
                last.members, model.members,
                "seed {seed}: executed member projection must equal the independent model"
            );
            assert_eq!(
                last.blocked, model.blocked,
                "seed {seed}: executed blocklist projection must equal the independent model"
            );
        });

    assert!(
        predicted.iter().any(|model| !model.members.is_empty()),
        "oracle is vacuous unless at least one seed predicts a non-empty member set"
    );
    assert!(
        predicted.iter().any(|model| !model.blocked.is_empty()),
        "oracle is vacuous unless at least one seed predicts a non-empty blocklist"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn maintenance_never_reports_a_failure() {
    let traces = join_all([1u64, 7, 13, 42, 99, 2026].map(|seed| knot_sim::run(seed, 16))).await;
    let failures = traces
        .iter()
        .flat_map(|trace| trace.steps.iter())
        .filter(|step| step.op == "maintain" && status_of(step) == Some(500))
        .count();
    assert_eq!(
        failures, 0,
        "maintenance must succeed on every repo simulation runs it against"
    );
    assert!(
        union_has(&traces, |step| step.op == "maintain"
            && status_of(step) == Some(200)),
        "maintenance path must actually run against a populated repo in at least one seed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_final_projection_state_is_seed_stable() {
    let first = knot_sim::run(2026, 16).await;
    let second = knot_sim::run(2026, 16).await;
    assert_eq!(
        first.snapshots, second.snapshots,
        "order-independent COB projections must converge to the same logical state"
    );
    let last = first.snapshots.last().expect("at least one round");
    assert!(
        last.repos.len() > 1,
        "simulation must have minted repos beyond the seed repo: {:?}",
        last.repos
    );
}
