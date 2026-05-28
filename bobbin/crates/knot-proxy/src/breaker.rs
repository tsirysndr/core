use std::sync::{Arc, Mutex};
use std::time::Duration;

use bobbin_runtime::Clock;
use thiserror::Error;
use tokio::time::Instant;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FailureThreshold(u32);

#[derive(Clone, Copy, Debug, Error)]
#[error("failure threshold must be at least 1")]
pub struct ThresholdError;

impl FailureThreshold {
    pub const fn new(n: u32) -> Result<Self, ThresholdError> {
        match n {
            0 => Err(ThresholdError),
            other => Ok(Self(other)),
        }
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug)]
enum BreakerState {
    Closed { failures: u32 },
    Open { until: Instant },
    HalfOpen,
}

#[derive(Clone, Copy, Debug, Error)]
#[error("circuit breaker open")]
pub struct CircuitOpen;

pub struct Breaker {
    state: Mutex<BreakerState>,
    threshold: FailureThreshold,
    cooldown: Duration,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for Breaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Breaker")
            .field("state", &self.state)
            .field("threshold", &self.threshold)
            .field("cooldown", &self.cooldown)
            .finish_non_exhaustive()
    }
}

impl Breaker {
    pub fn new(threshold: FailureThreshold, cooldown: Duration, clock: Arc<dyn Clock>) -> Self {
        Self {
            state: Mutex::new(BreakerState::Closed { failures: 0 }),
            threshold,
            cooldown,
            clock,
        }
    }

    pub fn try_acquire(self: &Arc<Self>) -> Result<BreakerPermit, CircuitOpen> {
        self.try_acquire_at(self.clock.now_instant())
    }

    fn try_acquire_at(self: &Arc<Self>, now: Instant) -> Result<BreakerPermit, CircuitOpen> {
        let mut state = self.state.lock().expect("breaker mutex poisoned");
        match *state {
            BreakerState::Closed { .. } => Ok(BreakerPermit::new(Arc::clone(self))),
            BreakerState::Open { until } if now >= until => {
                *state = BreakerState::HalfOpen;
                Ok(BreakerPermit::new(Arc::clone(self)))
            }
            BreakerState::Open { .. } | BreakerState::HalfOpen => Err(CircuitOpen),
        }
    }

    pub fn record_success(&self) {
        let mut state = self.state.lock().expect("breaker mutex poisoned");
        *state = BreakerState::Closed { failures: 0 };
    }

    pub fn record_failure(&self) {
        self.record_failure_at(self.clock.now_instant());
    }

    fn record_failure_at(&self, now: Instant) {
        let mut state = self.state.lock().expect("breaker mutex poisoned");
        let next = match *state {
            BreakerState::Closed { failures } => {
                let bumped = failures.saturating_add(1);
                if bumped >= self.threshold.get() {
                    BreakerState::Open {
                        until: now + self.cooldown,
                    }
                } else {
                    BreakerState::Closed { failures: bumped }
                }
            }
            BreakerState::HalfOpen => BreakerState::Open {
                until: now + self.cooldown,
            },
            BreakerState::Open { .. } => *state,
        };
        *state = next;
    }
}

#[must_use = "permit must outlive the upstream call so failures can be recorded"]
#[derive(Debug)]
pub struct BreakerPermit {
    breaker: Arc<Breaker>,
    resolved: bool,
}

impl BreakerPermit {
    fn new(breaker: Arc<Breaker>) -> Self {
        Self {
            breaker,
            resolved: false,
        }
    }

    pub fn record_success(mut self) {
        self.resolved = true;
        self.breaker.record_success();
    }

    pub fn record_failure(mut self) {
        self.resolved = true;
        self.breaker.record_failure();
    }
}

impl Drop for BreakerPermit {
    fn drop(&mut self) {
        if !self.resolved {
            self.breaker.record_success();
        }
    }
}

#[cfg(test)]
impl BreakerPermit {
    fn record_failure_at(mut self, now: Instant) {
        self.resolved = true;
        self.breaker.record_failure_at(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobbin_runtime::SystemClock;

    fn breaker(threshold: u32, cooldown_ms: u64) -> Arc<Breaker> {
        Arc::new(Breaker::new(
            FailureThreshold::new(threshold).unwrap(),
            Duration::from_millis(cooldown_ms),
            Arc::new(SystemClock::new()),
        ))
    }

    #[test]
    fn closed_breaker_admits_all() {
        let b = breaker(3, 100);
        b.try_acquire().unwrap().record_success();
        b.try_acquire().unwrap().record_success();
        b.try_acquire().unwrap().record_success();
    }

    #[test]
    fn opens_at_threshold() {
        let b = breaker(2, 1_000);
        b.record_failure();
        b.record_failure();
        assert!(b.try_acquire().is_err(), "must be open after 2 failures");
    }

    #[test]
    fn success_resets_failure_count() {
        let b = breaker(2, 1_000);
        b.record_failure();
        b.record_success();
        b.record_failure();
        b.try_acquire()
            .expect("successful run between failures must reset count")
            .record_success();
    }

    #[test]
    fn cooldown_admits_one_trial_only() {
        let b = breaker(1, 50);
        let t0 = Instant::now();
        let after = t0 + Duration::from_millis(60);
        b.record_failure_at(t0);
        assert!(b.try_acquire_at(t0).is_err());
        let trial = b
            .try_acquire_at(after)
            .expect("trial admitted after cooldown");
        assert!(
            b.try_acquire_at(after).is_err(),
            "second concurrent half-open call must be rejected",
        );
        trial.record_success();
    }

    #[test]
    fn half_open_failure_reopens() {
        let b = breaker(1, 50);
        let t0 = Instant::now();
        let after = t0 + Duration::from_millis(60);
        b.record_failure_at(t0);
        b.try_acquire_at(after)
            .expect("trial admitted")
            .record_failure_at(after);
        assert!(
            b.try_acquire_at(after).is_err(),
            "half-open failure must reopen breaker",
        );
    }

    #[test]
    fn half_open_success_closes() {
        let b = breaker(1, 50);
        let t0 = Instant::now();
        let after = t0 + Duration::from_millis(60);
        b.record_failure_at(t0);
        b.try_acquire_at(after)
            .expect("trial admitted")
            .record_success();
        b.try_acquire_at(after).expect("closed").record_success();
        b.try_acquire_at(after).expect("closed").record_success();
    }

    #[test]
    fn open_failure_does_not_extend_cooldown_indefinitely() {
        let b = breaker(1, 50);
        b.record_failure();
        let mid = Instant::now();
        b.record_failure();
        b.try_acquire_at(mid + Duration::from_millis(60))
            .expect("second failure while open must not push cooldown out")
            .record_success();
    }

    #[test]
    fn dropped_permit_counts_as_success() {
        let b = breaker(2, 1_000);
        b.record_failure();
        drop(b.try_acquire().expect("admitted"));
        b.try_acquire()
            .expect("dropped permit must reset failure count")
            .record_success();
    }

    #[test]
    fn dropped_half_open_permit_closes_breaker() {
        let b = breaker(1, 50);
        let t0 = Instant::now();
        let after = t0 + Duration::from_millis(60);
        b.record_failure_at(t0);
        drop(b.try_acquire_at(after).expect("trial admitted"));
        b.try_acquire_at(after)
            .expect("dropped half-open permit must close breaker")
            .record_success();
    }
}
