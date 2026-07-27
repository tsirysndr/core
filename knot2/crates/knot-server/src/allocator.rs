use std::time::Duration;

use knot_resource::DecayMs;
use tokio_util::sync::CancellationToken;

const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
const BACKGROUND_THREAD: &[u8] = b"background_thread\0";
const NARENAS: &[u8] = b"arenas.narenas\0";
const DIRTY_DECAY_NEW_ARENAS: &[u8] = b"arenas.dirty_decay_ms\0";

#[derive(Clone, Copy)]
struct ArenaIndex(u32);

impl ArenaIndex {
    fn write_decay(self, decay: DecayMs) -> bool {
        let key = format!("arena.{}.dirty_decay_ms\0", self.0);
        unsafe { tikv_jemalloc_ctl::raw::write(key.as_bytes(), decay.ms()) }.is_ok()
    }
}

#[derive(Clone, Copy)]
struct DecayCoverage {
    reached: usize,
    skipped: usize,
}

impl DecayCoverage {
    const EMPTY: Self = Self {
        reached: 0,
        skipped: 0,
    };

    fn record(self, reached: bool) -> Self {
        match reached {
            true => Self {
                reached: self.reached + 1,
                ..self
            },
            false => Self {
                skipped: self.skipped + 1,
                ..self
            },
        }
    }
}

fn write_or_warn<T>(name: &[u8], value: T, control: &str) {
    if let Err(error) = unsafe { tikv_jemalloc_ctl::raw::write(name, value) } {
        tracing::warn!(%error, control, "jemalloc control unavailable");
    }
}

fn apply_decay(decay: DecayMs) -> tikv_jemalloc_ctl::Result<DecayCoverage> {
    unsafe { tikv_jemalloc_ctl::raw::write(DIRTY_DECAY_NEW_ARENAS, decay.ms())? };
    let narenas: u32 = unsafe { tikv_jemalloc_ctl::raw::read(NARENAS)? };
    Ok((0..narenas)
        .map(ArenaIndex)
        .map(|arena| arena.write_decay(decay))
        .fold(DecayCoverage::EMPTY, DecayCoverage::record))
}

pub fn install() {
    match apply_decay(knot_resource::target_decay()) {
        Ok(coverage) => tracing::info!(
            arenas_reached = coverage.reached,
            arenas_skipped = coverage.skipped,
            "jemalloc dirty_decay applied to reachable arenas"
        ),
        Err(error) => tracing::warn!(%error, "jemalloc dirty_decay governor unavailable"),
    }
    write_or_warn(BACKGROUND_THREAD, true, "background_thread");
    let background: bool =
        unsafe { tikv_jemalloc_ctl::raw::read(b"opt.background_thread\0") }.unwrap_or(false);
    let retain: bool = unsafe { tikv_jemalloc_ctl::raw::read(b"opt.retain\0") }.unwrap_or(true);
    let dirty_decay_ms: isize =
        unsafe { tikv_jemalloc_ctl::raw::read(b"arena.0.dirty_decay_ms\0") }.unwrap_or(-1);
    tracing::info!(
        background_thread = background,
        retain,
        dirty_decay_ms,
        "jemalloc page-return configured"
    );
}

pub async fn govern_decay(shutdown: CancellationToken) {
    let mut ticker = tokio::time::interval(SAMPLE_INTERVAL);
    let mut applied = knot_resource::target_decay();
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            _ = ticker.tick() => {
                let target = knot_resource::target_decay();
                if knot_resource::decay_warrants_apply(applied, target) {
                    match apply_decay(target) {
                        Ok(coverage) => tracing::info!(
                            target_ms = target.ms(),
                            arenas_reached = coverage.reached,
                            arenas_skipped = coverage.skipped,
                            "jemalloc dirty_decay retuned"
                        ),
                        Err(error) => tracing::warn!(%error, "jemalloc dirty_decay retune failed"),
                    }
                    applied = target;
                }
                if knot_resource::cache_shed_warranted()
                    && let Some(freed) = knot_cache::reclaim_largest()
                {
                    tracing::warn!(
                        freed_bytes = freed.get(),
                        "shedding largest cache under memory pressure"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_decay_reaches_reachable_arenas_and_persists() {
        let narenas: u32 =
            unsafe { tikv_jemalloc_ctl::raw::read(NARENAS) }.expect("arenas.narenas is readable");
        assert!(narenas >= 1, "a live jemalloc has at least one arena");

        let coverage = apply_decay(knot_resource::target_decay())
            .expect("new-arena default write and narenas read succeed");
        assert!(
            coverage.reached >= 1,
            "arena 0 is always initialized, so the sweep reaches at least one arena"
        );
        assert_eq!(
            coverage.reached + coverage.skipped,
            narenas as usize,
            "every arena index is accounted as reached or skipped"
        );

        unsafe {
            tikv_jemalloc_ctl::raw::write(b"arena.0.dirty_decay_ms\0", 5_000isize)
                .expect("per-arena dirty_decay_ms is writable");
            let back: isize = tikv_jemalloc_ctl::raw::read(b"arena.0.dirty_decay_ms\0")
                .expect("per-arena dirty_decay_ms is readable");
            assert_eq!(back, 5_000, "a per-arena write reads back");
        }
    }
}
