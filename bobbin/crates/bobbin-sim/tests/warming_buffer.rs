use std::num::NonZeroUsize;
use std::time::Duration;

use bobbin_sim::workloads::{SlingshotFlap, SlingshotFlapConfig};
use bobbin_sim::{Sim, SimConfig, SimOutcome};
use tokio::runtime::Builder as TokioBuilder;

#[test]
fn lever_b_absorbs_sustained_brownout_without_disconnect() {
    let cfg = SlingshotFlapConfig {
        cross_did_stars: 5000,
        normal_latency_ms: 2,
        brownout_start_ms: 0,
        brownout_duration_ms: 60_000,
        brownout_latency_ms: 200,
        brownout_enabled: true,
        hydrant_send_timeout_ms: 30_000,
        hydrant_frame_pace_us: 1_000,
        omit_target_repos: false,
        emit_live_promotion_frame: false,
    };
    let mut sim_config = SimConfig::new(7);
    sim_config.parallelism = NonZeroUsize::new(4).unwrap();
    sim_config.max_virtual_runtime = Duration::from_secs(600);

    let workload = Box::new(SlingshotFlap::new(cfg.clone()));
    let runtime = TokioBuilder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("build current_thread runtime");
    let report = runtime.block_on(Sim::new(sim_config, workload).run());

    let total_frames = (cfg.cross_did_stars as u64) * 2;
    assert_eq!(
        report.outcome,
        SimOutcome::Passed,
        "Lever B should keep slingshot off the hot path during cold replay, got {:?} reason={:?}",
        report.outcome,
        report.failure_reason,
    );
    assert_eq!(report.events_processed, total_frames);
    assert_eq!(
        report.disconnect_count, 0,
        "expected zero disconnects under sustained brownout once Lever B parks the cohort, got {} (last={:?})",
        report.disconnect_count, report.last_disconnect,
    );

    let b = report.warming_buffer;
    assert_eq!(
        b.enqueued_total, cfg.cross_did_stars as u64,
        "every cross-DID star must park exactly once: {b:?}",
    );
    assert_eq!(
        b.drained_observe_total, cfg.cross_did_stars as u64,
        "every observed repo must drain its parked star: {b:?}",
    );
    assert_eq!(b.drained_promote_total, 0, "no residual at promote: {b:?}");
    assert_eq!(b.current_entries, 0, "buffer empty after drain: {b:?}");
    assert_eq!(b.evicted_total, 0, "no evictions in this workload: {b:?}");
    assert!(
        b.max_concurrent_entries > 0 && b.max_concurrent_entries <= cfg.cross_did_stars as u64,
        "peak depth bounded by cohort size, got {} for {} stars",
        b.max_concurrent_entries,
        cfg.cross_did_stars,
    );
}

#[test]
fn shadow_and_buffer_observe_the_same_population() {
    let cfg = SlingshotFlapConfig {
        cross_did_stars: 2000,
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

    assert_eq!(
        report.outcome,
        SimOutcome::Passed,
        "{:?}",
        report.failure_reason
    );

    let s = report.warming_shadow;
    let b = report.warming_buffer;
    assert_eq!(
        s.enqueued_total, b.dep_enqueued_total,
        "shadow note_unresolved per dep must equal buffer dep_enqueued_total: shadow={s:?} buffer={b:?}",
    );
    assert_eq!(
        s.drained_via_observe_total, b.dep_drained_observe_total,
        "shadow note_observed per dep must equal buffer dep_drained_observe_total: shadow={s:?} buffer={b:?}",
    );
    assert_eq!(
        s.distinct_keys_seen, b.distinct_keys_seen,
        "shadow and buffer must see the same distinct (owner, rkey) keys: shadow={s:?} buffer={b:?}",
    );
    assert_eq!(
        s.residual,
        b.dep_enqueued_total - b.dep_drained_observe_total,
        "shadow residual must equal buffer un-observed deps: shadow={s:?} buffer={b:?}",
    );
}

#[test]
fn warming_to_ready_promote_drains_residual_via_parallel_slingshot_wave() {
    let cfg = SlingshotFlapConfig {
        cross_did_stars: 200,
        normal_latency_ms: 2,
        brownout_start_ms: 0,
        brownout_duration_ms: 0,
        brownout_latency_ms: 0,
        brownout_enabled: false,
        hydrant_send_timeout_ms: 30_000,
        hydrant_frame_pace_us: 1_000,
        omit_target_repos: true,
        emit_live_promotion_frame: true,
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

    assert_eq!(
        report.outcome,
        SimOutcome::Passed,
        "promote-flush workload must complete, got {:?} reason={:?}",
        report.outcome,
        report.failure_reason,
    );

    let b = report.warming_buffer;
    let stars = cfg.cross_did_stars as u64;
    assert_eq!(
        b.enqueued_total, stars,
        "every cross-DID star must park (no repos arrive): {b:?}",
    );
    assert_eq!(
        b.drained_observe_total, 0,
        "no repos arrive on the stream so observe-drain stays at zero: {b:?}",
    );
    assert_eq!(
        b.drained_promote_total, stars,
        "Warming->Ready promote must drain every parked entry: {b:?}",
    );
    assert_eq!(
        b.current_entries, 0,
        "buffer must be empty after promote drain: {b:?}",
    );
    assert!(
        report.resolver_hits + report.resolver_misses >= stars,
        "promote wave must fan out one slingshot resolve per distinct dep, got hits={} misses={} for {stars} stars",
        report.resolver_hits,
        report.resolver_misses,
    );
    assert_eq!(
        report.edge_count,
        stars * 2 + 1,
        "each drained star contributes a primary edge plus a sh.tangled.feed.star.by mirror edge; promoter repo adds one. got {} for {stars} stars",
        report.edge_count,
    );
}

#[test]
fn buffer_enabled_and_disabled_produce_identical_edge_index() {
    let cfg = SlingshotFlapConfig {
        cross_did_stars: 200,
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

    let run_with_buffer = |enabled: bool| {
        let mut sim_config = SimConfig::new(7);
        sim_config.parallelism = NonZeroUsize::new(16).unwrap();
        sim_config.max_virtual_runtime = Duration::from_secs(120);
        sim_config.warming_buffer_enabled = enabled;
        let workload = Box::new(SlingshotFlap::new(cfg.clone()));
        let runtime = TokioBuilder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("build current_thread runtime");
        runtime.block_on(Sim::new(sim_config, workload).run())
    };

    let with_buffer = run_with_buffer(true);
    let without_buffer = run_with_buffer(false);

    assert_eq!(
        with_buffer.outcome,
        SimOutcome::Passed,
        "{:?}",
        with_buffer.failure_reason
    );
    assert_eq!(
        without_buffer.outcome,
        SimOutcome::Passed,
        "{:?}",
        without_buffer.failure_reason
    );
    assert_eq!(
        with_buffer.events_processed, without_buffer.events_processed,
        "events_processed must match across buffer modes",
    );
    assert_eq!(
        with_buffer.edge_count, without_buffer.edge_count,
        "edge_count must match: lever B is correctness-preserving (with={} without={})",
        with_buffer.edge_count, without_buffer.edge_count,
    );
    assert_eq!(
        with_buffer.last_cursor, without_buffer.last_cursor,
        "final cursor must match across buffer modes",
    );
}
