use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use knot_types::UnixMicros;

pub trait Clock: Send + Sync + 'static {
    fn now_unix_micros(&self) -> UnixMicros;
}

impl<T: Clock + ?Sized> Clock for Arc<T> {
    fn now_unix_micros(&self) -> UnixMicros {
        (**self).now_unix_micros()
    }
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_micros(&self) -> UnixMicros {
        let micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_micros() as u64)
            .unwrap_or(0);
        UnixMicros::new(micros)
    }
}

pub struct ManualClock {
    micros: AtomicU64,
}

impl ManualClock {
    pub fn new(start: UnixMicros) -> Self {
        Self {
            micros: AtomicU64::new(start.get()),
        }
    }

    pub fn advance(&self, delta: Duration) {
        self.micros
            .fetch_add(delta.as_micros() as u64, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_unix_micros(&self) -> UnixMicros {
        UnixMicros::new(self.micros.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_advances() {
        let clock = ManualClock::new(UnixMicros::new(1_000));
        assert_eq!(clock.now_unix_micros().get(), 1_000);
        clock.advance(Duration::from_micros(500));
        assert_eq!(clock.now_unix_micros().get(), 1_500);
    }

    #[test]
    fn manual_clock_same_start_same_sequence() {
        let one = ManualClock::new(UnixMicros::new(1_000));
        let two = ManualClock::new(UnixMicros::new(1_000));
        let advances = [10, 250, 7, 1_000];
        advances.iter().for_each(|&step| {
            one.advance(Duration::from_micros(step));
            two.advance(Duration::from_micros(step));
            assert_eq!(one.now_unix_micros(), two.now_unix_micros());
        });
    }

    #[test]
    fn a_shared_clock_advances_through_the_trait_object() {
        let shared = Arc::new(ManualClock::new(UnixMicros::new(1_000)));
        let view: Arc<dyn Clock> = Arc::clone(&shared) as Arc<dyn Clock>;
        assert_eq!(view.now_unix_micros().get(), 1_000);
        shared.advance(Duration::from_micros(250));
        assert_eq!(view.now_unix_micros().get(), 1_250);
    }
}
