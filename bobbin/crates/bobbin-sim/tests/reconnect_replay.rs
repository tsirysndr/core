use std::num::NonZeroUsize;
use std::time::Duration;

use bobbin_ingest::DisconnectKind;
use bobbin_sim::workloads::{SlingshotFlap, SlingshotFlapConfig};
use bobbin_sim::{Sim, SimConfig, SimOutcome};
use tokio::runtime::Builder as TokioBuilder;

#[test]
fn slingshot_flap_resumes_via_replay_after_pong_timeout() {
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
    sim_config.warming_buffer_enabled = false;

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
        "expected eventual recovery via replay after disconnect, got {:?} reason={:?}",
        report.outcome,
        report.failure_reason,
    );
    assert_eq!(
        report.events_processed, total_frames,
        "expected all {total_frames} frames processed after reconnect, got {}",
        report.events_processed,
    );
    assert!(
        report.disconnect_count >= 1,
        "expected at least one disconnect during sustained brownout, got {}",
        report.disconnect_count,
    );
    let last = report
        .last_disconnect
        .as_ref()
        .expect("last_disconnect populated when disconnect_count > 0");
    assert_eq!(
        last.kind,
        DisconnectKind::PongTimeout,
        "expected PongTimeout under sustained brownout (paced upstream + N=4 + 200ms slingshot RTT), got {:?}: {}",
        last.kind,
        last.message,
    );
    assert!(
        last.last_cursor.raw() > 0 && last.last_cursor.raw() < total_frames,
        "expected disconnect mid-stream, got last_cursor={}",
        last.last_cursor.raw(),
    );
}
