use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::time::Instant;

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UnixMicros(u64);

impl UnixMicros {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

pub type SleepFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

pub trait Clock: Send + Sync + 'static {
    fn now_unix_micros(&self) -> UnixMicros;
    fn now_instant(&self) -> Instant;
    fn sleep(&self, duration: Duration) -> SleepFuture;
    fn sleep_until(&self, deadline: Instant) -> SleepFuture;
}

#[derive(Clone, Copy, Debug)]
pub struct SystemClock {
    base_unix: UnixMicros,
    base_instant: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        let raw = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_micros();
        Self {
            base_unix: UnixMicros::new(u64::try_from(raw).unwrap_or(u64::MAX)),
            base_instant: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_unix_micros(&self) -> UnixMicros {
        let elapsed = self
            .now_instant()
            .saturating_duration_since(self.base_instant);
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        UnixMicros::new(self.base_unix.raw().saturating_add(micros))
    }

    fn now_instant(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) -> SleepFuture {
        Box::pin(tokio::time::sleep(duration))
    }

    fn sleep_until(&self, deadline: Instant) -> SleepFuture {
        Box::pin(tokio::time::sleep_until(deadline))
    }
}

#[derive(Clone, Debug)]
pub struct SimClock {
    base_unix: UnixMicros,
    base_instant: Instant,
}

impl SimClock {
    pub fn at(base_unix: UnixMicros) -> Self {
        Self {
            base_unix,
            base_instant: Instant::now(),
        }
    }
}

impl Clock for SimClock {
    fn now_unix_micros(&self) -> UnixMicros {
        let elapsed = self
            .now_instant()
            .saturating_duration_since(self.base_instant);
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        UnixMicros::new(self.base_unix.raw().saturating_add(micros))
    }

    fn now_instant(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) -> SleepFuture {
        Box::pin(tokio::time::sleep(duration))
    }

    fn sleep_until(&self, deadline: Instant) -> SleepFuture {
        Box::pin(tokio::time::sleep_until(deadline))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct ManualClock {
        base: Instant,
        unix_micros: AtomicU64,
    }

    impl ManualClock {
        fn new(unix_micros: u64) -> Self {
            Self {
                base: Instant::now(),
                unix_micros: AtomicU64::new(unix_micros),
            }
        }

        fn advance(&self, micros: u64) {
            self.unix_micros.fetch_add(micros, Ordering::SeqCst);
        }
    }

    impl Clock for ManualClock {
        fn now_unix_micros(&self) -> UnixMicros {
            UnixMicros::new(self.unix_micros.load(Ordering::SeqCst))
        }

        fn now_instant(&self) -> Instant {
            self.base + Duration::from_micros(self.unix_micros.load(Ordering::SeqCst))
        }

        fn sleep(&self, _: Duration) -> SleepFuture {
            Box::pin(std::future::ready(()))
        }

        fn sleep_until(&self, _: Instant) -> SleepFuture {
            Box::pin(std::future::ready(()))
        }
    }

    #[tokio::test]
    async fn external_clock_works_behind_dyn_arc() {
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(42));
        assert_eq!(clock.now_unix_micros().raw(), 42);
        let before = clock.now_instant();
        clock.sleep(Duration::from_secs(60)).await;
        assert_eq!(
            clock.now_instant(),
            before,
            "manual clock's sleep must not advance virtual time on its own",
        );
    }

    #[tokio::test]
    async fn manual_clock_advance_moves_both_unix_and_instant() {
        let clock = Arc::new(ManualClock::new(1_000));
        let dyn_clock: Arc<dyn Clock> = clock.clone();
        let unix_before = dyn_clock.now_unix_micros().raw();
        let instant_before = dyn_clock.now_instant();
        clock.advance(500);
        assert_eq!(dyn_clock.now_unix_micros().raw(), unix_before + 500);
        assert_eq!(
            dyn_clock.now_instant(),
            instant_before + Duration::from_micros(500),
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sim_clock_anchors_unix_at_explicit_base_under_paused_time() {
        let base = UnixMicros::new(1_700_000_000_000_000);
        let clock = SimClock::at(base);
        assert_eq!(
            clock.now_unix_micros(),
            base,
            "sim clock unix axis must reflect the explicit base, not wall clock",
        );
        let bump = Duration::from_secs(10);
        tokio::time::advance(bump).await;
        assert_eq!(
            clock.now_unix_micros().raw(),
            base.raw() + u64::try_from(bump.as_micros()).unwrap(),
            "sim clock unix axis must follow tokio virtual advance",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sim_clock_two_constructions_with_same_base_align() {
        let base = UnixMicros::new(2_000_000_000_000_000);
        let a = SimClock::at(base);
        let b = SimClock::at(base);
        assert_eq!(a.now_unix_micros(), b.now_unix_micros());
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(
            a.now_unix_micros(),
            b.now_unix_micros(),
            "two sim clocks anchored at the same base must agree on virtual time",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn system_clock_axes_advance_together_under_paused_time() {
        let clock = SystemClock::new();
        let unix_before = clock.now_unix_micros().raw();
        let instant_before = clock.now_instant();
        let bump = Duration::from_secs(60);
        tokio::time::advance(bump).await;
        let unix_after = clock.now_unix_micros().raw();
        let instant_after = clock.now_instant();
        assert_eq!(
            instant_after.saturating_duration_since(instant_before),
            bump,
            "instant axis must reflect virtual advance",
        );
        assert_eq!(
            unix_after - unix_before,
            u64::try_from(bump.as_micros()).unwrap(),
            "unix axis must follow the same virtual advance, not wall clock",
        );
    }
}
