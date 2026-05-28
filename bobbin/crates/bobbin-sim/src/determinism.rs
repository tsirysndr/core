use std::num::NonZeroUsize;
use std::time::Duration;

use bobbin_ingest::{DisconnectSnapshot, WarmingBufferSnapshot, WarmingShadowSnapshot};
use tokio::runtime::Builder as TokioBuilder;
use tracing_subscriber::Registry;
use tracing_subscriber::layer::SubscriberExt;

use crate::report::{SimOutcome, SimReport};
use crate::runtime::{Sim, SimConfig};
use crate::trace_capture::TraceCapture;
use crate::workload::Workload;

#[derive(Clone, Debug)]
pub struct LeakRunConfig {
    pub seed: u64,
    pub parallelism: NonZeroUsize,
    pub max_virtual_runtime: Duration,
    pub mem_ws_capacity: usize,
    pub warming_buffer_enabled: bool,
}

impl LeakRunConfig {
    pub fn new(
        seed: u64,
        parallelism: NonZeroUsize,
        max_virtual_runtime: Duration,
        mem_ws_capacity: usize,
    ) -> Self {
        Self {
            seed,
            parallelism,
            max_virtual_runtime,
            mem_ws_capacity,
            warming_buffer_enabled: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LeakOutcome {
    Match,
    OutcomeMismatch {
        first: SimOutcome,
        second: SimOutcome,
    },
    EventCountMismatch {
        first: u64,
        second: u64,
    },
    EdgeCountMismatch {
        first: u64,
        second: u64,
    },
    LastCursorMismatch {
        first: u64,
        second: u64,
    },
    ResolverHitsMismatch {
        first: u64,
        second: u64,
    },
    ResolverMissesMismatch {
        first: u64,
        second: u64,
    },
    ConsumerTooSlowMismatch {
        first: u64,
        second: u64,
    },
    DisconnectCountMismatch {
        first: u64,
        second: u64,
    },
    LastDisconnectMismatch {
        first: Option<DisconnectSnapshot>,
        second: Option<DisconnectSnapshot>,
    },
    WarmingShadowMismatch {
        first: WarmingShadowSnapshot,
        second: WarmingShadowSnapshot,
    },
    WarmingBufferMismatch {
        first: WarmingBufferSnapshot,
        second: WarmingBufferSnapshot,
    },
    TraceLengthMismatch {
        first: usize,
        second: usize,
    },
    TraceLineMismatch {
        index: usize,
        first: String,
        second: String,
    },
}

#[derive(Debug)]
pub struct LeakRunResult {
    pub config: LeakRunConfig,
    pub workload: &'static str,
    pub outcome: LeakOutcome,
    pub first_report: SimReport,
    pub second_report: SimReport,
}

impl LeakRunResult {
    pub fn passed(&self) -> bool {
        matches!(self.outcome, LeakOutcome::Match)
    }
}

pub fn run_leak_check<F>(config: LeakRunConfig, workload_factory: F) -> LeakRunResult
where
    F: Fn() -> Box<dyn Workload>,
{
    let workload_name = {
        let probe = workload_factory();
        probe.name()
    };

    let (first_report, first_lines) = run_once(&config, workload_factory());
    let (second_report, second_lines) = run_once(&config, workload_factory());

    let outcome = compare_runs(&first_report, &first_lines, &second_report, &second_lines);

    LeakRunResult {
        config,
        workload: workload_name,
        outcome,
        first_report,
        second_report,
    }
}

fn run_once(config: &LeakRunConfig, workload: Box<dyn Workload>) -> (SimReport, Vec<String>) {
    let capture = TraceCapture::new();
    let subscriber = Registry::default().with(capture.layer());

    let mut sim_config = SimConfig::new(config.seed);
    sim_config.max_virtual_runtime = config.max_virtual_runtime;
    sim_config.parallelism = config.parallelism;
    sim_config.mem_ws_capacity = config.mem_ws_capacity;
    sim_config.warming_buffer_enabled = config.warming_buffer_enabled;

    let runtime = TokioBuilder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("build current_thread runtime with paused time");

    let report = tracing::subscriber::with_default(subscriber, || {
        runtime.block_on(Sim::new(sim_config, workload).run())
    });
    drop(runtime);

    let lines = capture.into_lines();
    (report, lines)
}

fn compare_runs(
    a: &SimReport,
    a_lines: &[String],
    b: &SimReport,
    b_lines: &[String],
) -> LeakOutcome {
    if a.outcome != b.outcome {
        return LeakOutcome::OutcomeMismatch {
            first: a.outcome,
            second: b.outcome,
        };
    }
    if a.events_processed != b.events_processed {
        return LeakOutcome::EventCountMismatch {
            first: a.events_processed,
            second: b.events_processed,
        };
    }
    if a.edge_count != b.edge_count {
        return LeakOutcome::EdgeCountMismatch {
            first: a.edge_count,
            second: b.edge_count,
        };
    }
    if a.last_cursor != b.last_cursor {
        return LeakOutcome::LastCursorMismatch {
            first: a.last_cursor,
            second: b.last_cursor,
        };
    }
    if a.resolver_hits != b.resolver_hits {
        return LeakOutcome::ResolverHitsMismatch {
            first: a.resolver_hits,
            second: b.resolver_hits,
        };
    }
    if a.resolver_misses != b.resolver_misses {
        return LeakOutcome::ResolverMissesMismatch {
            first: a.resolver_misses,
            second: b.resolver_misses,
        };
    }
    if a.consumer_too_slow_count != b.consumer_too_slow_count {
        return LeakOutcome::ConsumerTooSlowMismatch {
            first: a.consumer_too_slow_count,
            second: b.consumer_too_slow_count,
        };
    }
    if a.disconnect_count != b.disconnect_count {
        return LeakOutcome::DisconnectCountMismatch {
            first: a.disconnect_count,
            second: b.disconnect_count,
        };
    }
    if a.last_disconnect != b.last_disconnect {
        return LeakOutcome::LastDisconnectMismatch {
            first: a.last_disconnect.clone(),
            second: b.last_disconnect.clone(),
        };
    }
    if a.warming_shadow != b.warming_shadow {
        return LeakOutcome::WarmingShadowMismatch {
            first: a.warming_shadow,
            second: b.warming_shadow,
        };
    }
    if a.warming_buffer != b.warming_buffer {
        return LeakOutcome::WarmingBufferMismatch {
            first: a.warming_buffer,
            second: b.warming_buffer,
        };
    }
    if a_lines.len() != b_lines.len() {
        return LeakOutcome::TraceLengthMismatch {
            first: a_lines.len(),
            second: b_lines.len(),
        };
    }
    for (i, (x, y)) in a_lines.iter().zip(b_lines.iter()).enumerate() {
        if x != y {
            return LeakOutcome::TraceLineMismatch {
                index: i,
                first: x.clone(),
                second: y.clone(),
            };
        }
    }
    LeakOutcome::Match
}
