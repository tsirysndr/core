use std::sync::Arc;
use std::time::Duration;

use bobbin_runtime::{Clock, MemoryBudget};
use bobbin_xrpc::{HeavyLimiter, PressureVerdict};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::cgroup;

pub struct AdaptiveThresholds {
    pub interval: Duration,
    pub relieve_below_ratio: f64,
    pub tighten_above_ratio: f64,
}

fn verdict(used_ratio: f64, thresholds: &AdaptiveThresholds) -> PressureVerdict {
    if used_ratio > thresholds.tighten_above_ratio {
        PressureVerdict::Tighten
    } else if used_ratio < thresholds.relieve_below_ratio {
        PressureVerdict::Relieve
    } else {
        PressureVerdict::Hold
    }
}

const PURGE_ALL_ARENAS: &[u8] = b"arena.4096.purge\0";

fn purge_arenas() {
    if let Err(e) = unsafe { tikv_jemalloc_ctl::raw::write(PURGE_ALL_ARENAS, ()) } {
        tracing::warn!(error = %e, "jemalloc arena purge failed");
    }
}

pub fn spawn(
    limiter: Arc<HeavyLimiter>,
    clock: Arc<dyn Clock>,
    budget: MemoryBudget,
    thresholds: AdaptiveThresholds,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let max = budget.bytes().max(1) as f64;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = clock.sleep(thresholds.interval) => {}
            }
            let Some(current) = cgroup::read_current() else {
                continue;
            };
            let used_ratio = current.bytes() as f64 / max;
            let decision = verdict(used_ratio, &thresholds);
            limiter.adjust(decision);
            if matches!(decision, PressureVerdict::Tighten) {
                purge_arenas();
            }
            tracing::debug!(
                used_ratio,
                limit = limiter.limit(),
                ?decision,
                "adaptive concurrency tick"
            );
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thresholds() -> AdaptiveThresholds {
        AdaptiveThresholds {
            interval: Duration::from_millis(500),
            relieve_below_ratio: 0.65,
            tighten_above_ratio: 0.80,
        }
    }

    #[test]
    fn over_tighten_line_tightens() {
        assert!(matches!(
            verdict(0.92, &thresholds()),
            PressureVerdict::Tighten
        ));
    }

    #[test]
    fn under_relieve_line_relieves() {
        assert!(matches!(
            verdict(0.40, &thresholds()),
            PressureVerdict::Relieve
        ));
    }

    #[test]
    fn between_lines_holds() {
        assert!(matches!(
            verdict(0.72, &thresholds()),
            PressureVerdict::Hold
        ));
    }
}
