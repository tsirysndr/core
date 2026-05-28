use std::num::NonZeroUsize;
use std::time::Duration;

use bobbin_sim::workloads::{SlingshotFlap, SlingshotFlapConfig};
use bobbin_sim::{Sim, SimConfig, SimOutcome};
use tokio::runtime::Builder as TokioBuilder;

#[test]
fn shadow_buffer_bounded_by_cross_did_attachment_cohort_and_drains_to_zero() {
    let cfg = SlingshotFlapConfig {
        cross_did_stars: 5000,
        normal_latency_ms: 2,
        brownout_start_ms: 0,
        brownout_duration_ms: 0,
        brownout_latency_ms: 0,
        brownout_enabled: false,
        hydrant_send_timeout_ms: 30_000,
        hydrant_frame_pace_us: 1_000,
        omit_target_repos: false,
        emit_live_promotion_frame: false,
    };
    let mut sim_config = SimConfig::new(7);
    sim_config.parallelism = NonZeroUsize::new(16).unwrap();
    sim_config.max_virtual_runtime = Duration::from_secs(120);

    let workload = Box::new(SlingshotFlap::new(cfg.clone()));
    let runtime = TokioBuilder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("build current_thread runtime");
    let report = runtime.block_on(Sim::new(sim_config, workload).run());

    let stars = cfg.cross_did_stars as u64;
    let total = stars * 2;

    assert_eq!(
        report.outcome,
        SimOutcome::Passed,
        "expected clean drain with brownout disabled, got {:?} reason={:?}",
        report.outcome,
        report.failure_reason,
    );
    assert_eq!(report.events_processed, total);

    let s = report.warming_shadow;
    assert_eq!(
        s.enqueued_total, stars,
        "every cross-DID star must enqueue exactly once: {s:?}",
    );
    assert_eq!(
        s.drained_via_observe_total, stars,
        "every observe must drain its corresponding enqueue (stars send their repos in this workload): {s:?}",
    );
    assert_eq!(
        s.distinct_keys_seen, stars,
        "one (owner, rkey) pair per repo: {s:?}",
    );
    assert_eq!(
        s.residual, 0,
        "no entry must remain pending after observe arrives: {s:?}",
    );
    assert!(
        s.max_concurrent > 0 && s.max_concurrent <= stars,
        "peak depth must be bounded by the cross-DID attachment cohort, got max_concurrent={} for {stars} stars: {s:?}",
        s.max_concurrent,
    );
}
