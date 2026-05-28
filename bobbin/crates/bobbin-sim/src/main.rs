use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use bobbin_sim::workloads::{
    CancelMidHydration, CancelMidHydrationConfig, ColdStartUnderLiveLoad,
    ColdStartUnderLiveLoadConfig, ConcurrentReadsDuringReplay, ConcurrentReadsDuringReplayConfig,
    FrameBurst, HydrantDisconnectBarrage, HydrantDisconnectBarrageConfig, SlingshotFlap,
    SlingshotFlapConfig,
};
use bobbin_sim::{
    LeakOutcome, LeakRunConfig, Sim, SimConfig, SimOutcome, SimReport, Workload, run_leak_check,
};
use clap::{Parser, ValueEnum};

#[derive(Parser, Debug)]
#[command(name = "bobbin-sim", version)]
struct Cli {
    #[arg(long, default_value_t = 0)]
    seed: u64,
    #[arg(long, value_enum, default_value_t = WorkloadName::FrameBurst)]
    workload: WorkloadName,
    #[arg(long, default_value_t = 64)]
    frames: usize,
    #[arg(long, default_value_t = 60)]
    max_virtual_seconds: u64,
    #[arg(long)]
    no_brownout: bool,
    #[arg(long, default_value_t = 200)]
    cross_did_stars: usize,
    #[arg(long, default_value_t = 1_000)]
    brownout_start_ms: u64,
    #[arg(long, default_value_t = 5_000)]
    brownout_duration_ms: u64,
    #[arg(long, default_value_t = 200)]
    brownout_latency_ms: u64,
    #[arg(long, default_value_t = 2)]
    normal_latency_ms: u64,
    #[arg(long, default_value_t = 16)]
    parallelism: usize,
    #[arg(long, default_value_t = 4096)]
    mem_ws_capacity: usize,
    #[arg(long, default_value_t = 30_000)]
    hydrant_send_timeout_ms: u64,
    #[arg(long, default_value_t = 0)]
    hydrant_frame_pace_us: u64,
    #[arg(long, default_value_t = 1)]
    seeds: u64,
    #[arg(long)]
    quarantine_out: Option<PathBuf>,
    #[arg(long)]
    leak_check: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum, Eq, PartialEq)]
enum WorkloadName {
    FrameBurst,
    SlingshotFlap,
    CancelMidHydration,
    HydrantDisconnectBarrage,
    ColdStartUnderLiveLoad,
    ConcurrentReadsDuringReplay,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,bobbin_sim=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    if cli.seeds <= 1 && !cli.leak_check {
        let report = run_single(&cli, cli.seed);
        println!(
            "{}",
            serde_json::to_string_pretty(&report_as_json(&report)).unwrap()
        );
        exit_code_from(report.outcome)
    } else if cli.leak_check && cli.seeds <= 1 {
        let result = run_leak(&cli, cli.seed);
        let json = leak_result_as_json(&result);
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
        if matches!(result.outcome, LeakOutcome::Match)
            && matches!(result.first_report.outcome, SimOutcome::Passed)
        {
            ExitCode::from(0)
        } else {
            ExitCode::from(1)
        }
    } else {
        run_sweep(&cli)
    }
}

fn run_sweep(cli: &Cli) -> ExitCode {
    let mut quarantined: Vec<serde_json::Value> = Vec::new();
    let mut passed = 0u64;
    let total = cli.seeds;
    for offset in 0..total {
        let seed = cli.seed.wrapping_add(offset);
        let outcome_record = if cli.leak_check {
            let result = run_leak(cli, seed);
            let leak_match = matches!(result.outcome, LeakOutcome::Match);
            let workload_passed = matches!(result.first_report.outcome, SimOutcome::Passed);
            let ok = leak_match && workload_passed;
            if ok {
                passed += 1;
                None
            } else {
                Some(leak_result_as_json(&result))
            }
        } else {
            let report = run_single(cli, seed);
            if matches!(report.outcome, SimOutcome::Passed) {
                passed += 1;
                None
            } else {
                Some(report_as_json(&report))
            }
        };
        if let Some(rec) = outcome_record {
            quarantined.push(rec);
        }
    }
    let summary = serde_json::json!({
        "workload": format!("{:?}", cli.workload),
        "seeds_run": total,
        "passed": passed,
        "failed": total - passed,
        "leak_check": cli.leak_check,
        "parallelism": cli.parallelism,
        "quarantine_count": quarantined.len(),
    });
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());
    if let Some(path) = &cli.quarantine_out {
        let payload = serde_json::json!({
            "summary": summary,
            "failures": quarantined,
        });
        std::fs::write(path, serde_json::to_vec_pretty(&payload).unwrap())
            .expect("write quarantine output");
        eprintln!("quarantine written to {}", path.display());
    }
    if quarantined.is_empty() {
        ExitCode::from(0)
    } else {
        ExitCode::from(1)
    }
}

fn run_single(cli: &Cli, seed: u64) -> SimReport {
    let workload = build_workload(cli);
    let mut config = SimConfig::new(seed);
    config.max_virtual_runtime = Duration::from_secs(cli.max_virtual_seconds);
    config.parallelism = NonZeroUsize::new(cli.parallelism).expect("parallelism > 0");
    config.mem_ws_capacity = cli.mem_ws_capacity;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("build current_thread runtime with paused time");
    runtime.block_on(Sim::new(config, workload).run())
}

fn run_leak(cli: &Cli, seed: u64) -> bobbin_sim::LeakRunResult {
    let leak_config = LeakRunConfig {
        seed,
        parallelism: NonZeroUsize::new(cli.parallelism).expect("parallelism > 0"),
        max_virtual_runtime: Duration::from_secs(cli.max_virtual_seconds),
        mem_ws_capacity: cli.mem_ws_capacity,
        warming_buffer_enabled: true,
    };
    let cli_snapshot = CliSnapshot::from(cli);
    let factory = move || -> Box<dyn Workload> { build_workload_from(&cli_snapshot) };
    run_leak_check(leak_config, factory)
}

#[derive(Clone)]
struct CliSnapshot {
    workload: WorkloadName,
    frames: usize,
    cross_did_stars: usize,
    normal_latency_ms: u64,
    brownout_start_ms: u64,
    brownout_duration_ms: u64,
    brownout_latency_ms: u64,
    brownout_enabled: bool,
    hydrant_send_timeout_ms: u64,
    hydrant_frame_pace_us: u64,
}

impl From<&Cli> for CliSnapshot {
    fn from(cli: &Cli) -> Self {
        Self {
            workload: cli.workload,
            frames: cli.frames,
            cross_did_stars: cli.cross_did_stars,
            normal_latency_ms: cli.normal_latency_ms,
            brownout_start_ms: cli.brownout_start_ms,
            brownout_duration_ms: cli.brownout_duration_ms,
            brownout_latency_ms: cli.brownout_latency_ms,
            brownout_enabled: !cli.no_brownout,
            hydrant_send_timeout_ms: cli.hydrant_send_timeout_ms,
            hydrant_frame_pace_us: cli.hydrant_frame_pace_us,
        }
    }
}

fn build_workload(cli: &Cli) -> Box<dyn Workload> {
    build_workload_from(&CliSnapshot::from(cli))
}

fn build_workload_from(snap: &CliSnapshot) -> Box<dyn Workload> {
    match snap.workload {
        WorkloadName::FrameBurst => Box::new(FrameBurst::new(snap.frames)),
        WorkloadName::SlingshotFlap => Box::new(SlingshotFlap::new(SlingshotFlapConfig {
            cross_did_stars: snap.cross_did_stars,
            normal_latency_ms: snap.normal_latency_ms,
            brownout_start_ms: snap.brownout_start_ms,
            brownout_duration_ms: snap.brownout_duration_ms,
            brownout_latency_ms: snap.brownout_latency_ms,
            brownout_enabled: snap.brownout_enabled,
            hydrant_send_timeout_ms: snap.hydrant_send_timeout_ms,
            hydrant_frame_pace_us: snap.hydrant_frame_pace_us,
            omit_target_repos: false,
            emit_live_promotion_frame: false,
        })),
        WorkloadName::CancelMidHydration => {
            Box::new(CancelMidHydration::new(CancelMidHydrationConfig::default()))
        }
        WorkloadName::HydrantDisconnectBarrage => Box::new(HydrantDisconnectBarrage::new(
            HydrantDisconnectBarrageConfig::default(),
        )),
        WorkloadName::ColdStartUnderLiveLoad => Box::new(ColdStartUnderLiveLoad::new(
            ColdStartUnderLiveLoadConfig::default(),
        )),
        WorkloadName::ConcurrentReadsDuringReplay => Box::new(ConcurrentReadsDuringReplay::new(
            ConcurrentReadsDuringReplayConfig::default(),
        )),
    }
}

fn report_as_json(r: &SimReport) -> serde_json::Value {
    serde_json::json!({
        "workload": r.workload,
        "seed": r.seed,
        "outcome": format!("{:?}", r.outcome),
        "virtual_runtime_micros": r.virtual_runtime.as_micros() as u64,
        "virtual_clock_end_unix_micros": r.virtual_clock_end.raw(),
        "events_processed": r.events_processed,
        "last_cursor": r.last_cursor,
        "edge_count": r.edge_count,
        "resolver_hits": r.resolver_hits,
        "resolver_misses": r.resolver_misses,
        "consumer_too_slow_count": r.consumer_too_slow_count,
        "disconnect_count": r.disconnect_count,
        "last_disconnect": r.last_disconnect.as_ref().map(|d| serde_json::json!({
            "kind": format!("{:?}", d.kind),
            "message": d.message,
            "at_unix_micros": d.at_unix_micros.raw(),
            "last_cursor": d.last_cursor.raw(),
        })),
        "warming_shadow": {
            "enqueued_total": r.warming_shadow.enqueued_total,
            "drained_via_observe_total": r.warming_shadow.drained_via_observe_total,
            "max_concurrent": r.warming_shadow.max_concurrent,
            "residual": r.warming_shadow.residual,
            "distinct_keys_seen": r.warming_shadow.distinct_keys_seen,
        },
        "warming_buffer": {
            "enqueued_total": r.warming_buffer.enqueued_total,
            "drained_observe_total": r.warming_buffer.drained_observe_total,
            "drained_promote_total": r.warming_buffer.drained_promote_total,
            "evicted_total": r.warming_buffer.evicted_total,
            "rejected_after_seal": r.warming_buffer.rejected_after_seal,
            "distinct_keys_seen": r.warming_buffer.distinct_keys_seen,
            "current_entries": r.warming_buffer.current_entries,
            "max_concurrent_entries": r.warming_buffer.max_concurrent_entries,
            "dep_enqueued_total": r.warming_buffer.dep_enqueued_total,
            "dep_drained_observe_total": r.warming_buffer.dep_drained_observe_total,
        },
        "failure_reason": r.failure_reason,
    })
}

fn leak_result_as_json(r: &bobbin_sim::LeakRunResult) -> serde_json::Value {
    serde_json::json!({
        "workload": r.workload,
        "seed": r.config.seed,
        "parallelism": r.config.parallelism.get(),
        "leak_outcome": leak_outcome_as_json(&r.outcome),
        "first": report_as_json(&r.first_report),
        "second": report_as_json(&r.second_report),
    })
}

fn leak_outcome_as_json(outcome: &LeakOutcome) -> serde_json::Value {
    match outcome {
        LeakOutcome::Match => serde_json::json!({ "kind": "match" }),
        LeakOutcome::OutcomeMismatch { first, second } => serde_json::json!({
            "kind": "outcome_mismatch",
            "first": format!("{first:?}"),
            "second": format!("{second:?}"),
        }),
        LeakOutcome::EventCountMismatch { first, second } => serde_json::json!({
            "kind": "event_count_mismatch",
            "first": first,
            "second": second,
        }),
        LeakOutcome::EdgeCountMismatch { first, second } => serde_json::json!({
            "kind": "edge_count_mismatch",
            "first": first,
            "second": second,
        }),
        LeakOutcome::LastCursorMismatch { first, second } => serde_json::json!({
            "kind": "last_cursor_mismatch",
            "first": first,
            "second": second,
        }),
        LeakOutcome::ResolverHitsMismatch { first, second } => serde_json::json!({
            "kind": "resolver_hits_mismatch",
            "first": first,
            "second": second,
        }),
        LeakOutcome::ResolverMissesMismatch { first, second } => serde_json::json!({
            "kind": "resolver_misses_mismatch",
            "first": first,
            "second": second,
        }),
        LeakOutcome::ConsumerTooSlowMismatch { first, second } => serde_json::json!({
            "kind": "consumer_too_slow_mismatch",
            "first": first,
            "second": second,
        }),
        LeakOutcome::DisconnectCountMismatch { first, second } => serde_json::json!({
            "kind": "disconnect_count_mismatch",
            "first": first,
            "second": second,
        }),
        LeakOutcome::LastDisconnectMismatch { first, second } => serde_json::json!({
            "kind": "last_disconnect_mismatch",
            "first": first.as_ref().map(|d| format!("{:?}: {}", d.kind, d.message)),
            "second": second.as_ref().map(|d| format!("{:?}: {}", d.kind, d.message)),
        }),
        LeakOutcome::WarmingShadowMismatch { first, second } => serde_json::json!({
            "kind": "warming_shadow_mismatch",
            "first": format!("{first:?}"),
            "second": format!("{second:?}"),
        }),
        LeakOutcome::WarmingBufferMismatch { first, second } => serde_json::json!({
            "kind": "warming_buffer_mismatch",
            "first": format!("{first:?}"),
            "second": format!("{second:?}"),
        }),
        LeakOutcome::TraceLengthMismatch { first, second } => serde_json::json!({
            "kind": "trace_length_mismatch",
            "first": first,
            "second": second,
        }),
        LeakOutcome::TraceLineMismatch {
            index,
            first,
            second,
        } => serde_json::json!({
            "kind": "trace_line_mismatch",
            "index": index,
            "first": first,
            "second": second,
        }),
    }
}

fn exit_code_from(outcome: SimOutcome) -> ExitCode {
    match outcome {
        SimOutcome::Passed => ExitCode::from(0),
        SimOutcome::Failed | SimOutcome::TimedOut => ExitCode::from(1),
    }
}
