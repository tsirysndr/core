use std::num::NonZeroUsize;
use std::time::Duration;

use bobbin_sim::workloads::{
    CancelMidHydration, CancelMidHydrationConfig, ColdStartUnderLiveLoad,
    ColdStartUnderLiveLoadConfig, ConcurrentReadsDuringReplay, ConcurrentReadsDuringReplayConfig,
    FrameBurst, HydrantDisconnectBarrage, HydrantDisconnectBarrageConfig, SlingshotFlap,
    SlingshotFlapConfig,
};
use bobbin_sim::{LeakOutcome, LeakRunConfig, Workload, run_leak_check};

const PARALLELISM_SWEEP: &[usize] = &[1, 4, 16, 64];
const SEEDS: &[u64] = &[1, 7, 42, 1337];

#[test]
fn frame_burst_is_byte_deterministic_across_parallelism_sweep() {
    for &par in PARALLELISM_SWEEP {
        for &seed in SEEDS {
            let parallelism = NonZeroUsize::new(par).unwrap();
            let config = LeakRunConfig {
                seed,
                parallelism,
                max_virtual_runtime: Duration::from_secs(30),
                mem_ws_capacity: 4096,
                warming_buffer_enabled: true,
            };
            let factory = || -> Box<dyn Workload> { Box::new(FrameBurst::new(64)) };
            let result = run_leak_check(config.clone(), factory);
            assert!(
                result.passed(),
                "frame-burst leak at par={par} seed={seed}: {:?}",
                result.outcome,
            );
            match &result.outcome {
                LeakOutcome::Match => {}
                other => panic!("non-match outcome despite passed(): {other:?}"),
            }
            assert_eq!(
                result.first_report.events_processed, 64,
                "frame-burst should process all 64 frames at par={par} seed={seed}",
            );
        }
    }
}

#[test]
fn cancel_mid_hydration_is_byte_deterministic() {
    for &par in PARALLELISM_SWEEP {
        for &seed in SEEDS {
            let parallelism = NonZeroUsize::new(par).unwrap();
            let config = LeakRunConfig {
                seed,
                parallelism,
                max_virtual_runtime: Duration::from_secs(60),
                mem_ws_capacity: 4096,
                warming_buffer_enabled: true,
            };
            let factory = || -> Box<dyn Workload> {
                Box::new(CancelMidHydration::new(CancelMidHydrationConfig {
                    frames: 200,
                    cancel_at_events: 50,
                    slingshot_latency_ms: 50,
                    frame_pace_us: 100,
                }))
            };
            let result = run_leak_check(config.clone(), factory);
            assert!(
                result.passed(),
                "cancel-mid-hydration leak at par={par} seed={seed}: {:?}",
                result.outcome,
            );
            match &result.outcome {
                LeakOutcome::Match => {}
                other => panic!("non-match outcome despite passed(): {other:?}"),
            }
        }
    }
}

#[test]
fn hydrant_disconnect_barrage_is_byte_deterministic() {
    for &par in PARALLELISM_SWEEP {
        for &seed in SEEDS {
            let parallelism = NonZeroUsize::new(par).unwrap();
            let config = LeakRunConfig {
                seed,
                parallelism,
                max_virtual_runtime: Duration::from_secs(60),
                mem_ws_capacity: 4096,
                warming_buffer_enabled: true,
            };
            let factory = || -> Box<dyn Workload> {
                Box::new(HydrantDisconnectBarrage::new(
                    HydrantDisconnectBarrageConfig {
                        frames: 256,
                        disconnect_after_frames_per_session: vec![50, 80, 60],
                    },
                ))
            };
            let result = run_leak_check(config.clone(), factory);
            assert!(
                result.passed(),
                "hydrant-disconnect-barrage leak at par={par} seed={seed}: {:?}",
                result.outcome,
            );
            assert_eq!(
                result.first_report.events_processed, 256,
                "expected all 256 frames processed",
            );
        }
    }
}

#[test]
fn cold_start_under_live_load_is_byte_deterministic() {
    for &par in PARALLELISM_SWEEP {
        for &seed in SEEDS {
            let parallelism = NonZeroUsize::new(par).unwrap();
            let config = LeakRunConfig {
                seed,
                parallelism,
                max_virtual_runtime: Duration::from_secs(60),
                mem_ws_capacity: 4096,
                warming_buffer_enabled: true,
            };
            let factory = || -> Box<dyn Workload> {
                Box::new(ColdStartUnderLiveLoad::new(ColdStartUnderLiveLoadConfig {
                    replay_frames: 200,
                    live_frames: 50,
                    live_pace_us: 1_000,
                    replay_pace_us: 0,
                }))
            };
            let result = run_leak_check(config.clone(), factory);
            assert!(
                result.passed(),
                "cold-start-under-live-load leak at par={par} seed={seed}: {:?}",
                result.outcome,
            );
            assert_eq!(
                result.first_report.events_processed, 250,
                "expected 250 (replay+live) frames",
            );
            assert_eq!(
                result.first_report.edge_count, 250,
                "diversified owners should yield 250 distinct edge keys",
            );
        }
    }
}

#[test]
fn concurrent_reads_during_replay_is_byte_deterministic() {
    for &par in PARALLELISM_SWEEP {
        for &seed in SEEDS {
            let parallelism = NonZeroUsize::new(par).unwrap();
            let config = LeakRunConfig {
                seed,
                parallelism,
                max_virtual_runtime: Duration::from_secs(60),
                mem_ws_capacity: 4096,
                warming_buffer_enabled: true,
            };
            let factory = || -> Box<dyn Workload> {
                Box::new(ConcurrentReadsDuringReplay::new(
                    ConcurrentReadsDuringReplayConfig {
                        follow_frames: 200,
                        frame_pace_us: 100,
                        read_pace_us: 1_000,
                    },
                ))
            };
            let result = run_leak_check(config.clone(), factory);
            assert!(
                result.passed(),
                "concurrent-reads-during-replay leak at par={par} seed={seed}: {:?}",
                result.outcome,
            );
            assert_eq!(
                result.first_report.events_processed, 200,
                "expected 200 follow frames processed",
            );
        }
    }
}

#[test]
fn slingshot_flap_short_brownout_is_byte_deterministic() {
    for &par in &[4usize, 16, 64] {
        for &seed in &[7u64, 99] {
            let parallelism = NonZeroUsize::new(par).unwrap();
            let config = LeakRunConfig {
                seed,
                parallelism,
                max_virtual_runtime: Duration::from_secs(30),
                mem_ws_capacity: 4096,
                warming_buffer_enabled: true,
            };
            let factory = || -> Box<dyn Workload> {
                Box::new(SlingshotFlap::new(SlingshotFlapConfig {
                    cross_did_stars: 50,
                    normal_latency_ms: 2,
                    brownout_start_ms: 0,
                    brownout_duration_ms: 100,
                    brownout_latency_ms: 50,
                    brownout_enabled: true,
                    hydrant_send_timeout_ms: 30_000,
                    hydrant_frame_pace_us: 0,
                    omit_target_repos: false,
                    emit_live_promotion_frame: false,
                }))
            };
            let result = run_leak_check(config.clone(), factory);
            assert!(
                result.passed(),
                "slingshot-flap leak at par={par} seed={seed}: {:?}",
                result.outcome,
            );
            assert_eq!(
                result.first_report.events_processed, 100,
                "expected 100 (50 stars + 50 repos) frames processed",
            );
            assert!(
                result.first_report.warming_buffer.enqueued_total > 0,
                "expected the buffer path to exercise during warming at par={par} seed={seed}, got snapshot={:?}",
                result.first_report.warming_buffer,
            );
            assert_eq!(
                result.first_report.warming_buffer.enqueued_total,
                result.first_report.warming_buffer.drained_observe_total,
                "every parked star must drain via observe at par={par} seed={seed}, got snapshot={:?}",
                result.first_report.warming_buffer,
            );
        }
    }
}
