use std::time::Duration;

use bobbin_ingest::{DisconnectSnapshot, WarmingBufferSnapshot, WarmingShadowSnapshot};
use bobbin_runtime::UnixMicros;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SimOutcome {
    Passed,
    Failed,
    TimedOut,
}

#[derive(Clone, Debug)]
pub struct SimReport {
    pub workload: &'static str,
    pub seed: u64,
    pub outcome: SimOutcome,
    pub virtual_runtime: Duration,
    pub virtual_clock_end: UnixMicros,
    pub events_processed: u64,
    pub last_cursor: u64,
    pub edge_count: u64,
    pub resolver_hits: u64,
    pub resolver_misses: u64,
    pub consumer_too_slow_count: u64,
    pub disconnect_count: u64,
    pub last_disconnect: Option<DisconnectSnapshot>,
    pub warming_shadow: WarmingShadowSnapshot,
    pub warming_buffer: WarmingBufferSnapshot,
    pub failure_reason: Option<String>,
}

impl SimReport {
    pub fn passed(self) -> bool {
        matches!(self.outcome, SimOutcome::Passed)
    }
}
