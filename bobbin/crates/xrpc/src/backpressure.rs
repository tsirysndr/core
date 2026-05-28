use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bobbin_runtime::MemoryBudget;

use crate::XrpcError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PerRequestAnonBytes(u64);

impl PerRequestAnonBytes {
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    pub const fn bytes(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReservedFloor(u64);

impl ReservedFloor {
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    pub const fn bytes(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaxInFlight(usize);

impl MaxInFlight {
    pub fn from_budget(
        budget: MemoryBudget,
        reserved: ReservedFloor,
        per_request: PerRequestAnonBytes,
    ) -> Self {
        let usable = budget.bytes().saturating_sub(reserved.bytes());
        let per = per_request.bytes().max(1);
        let slots = (usable / per).max(1);
        Self(usize::try_from(slots).unwrap_or(usize::MAX))
    }

    pub const fn get(self) -> usize {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PressureVerdict {
    Tighten,
    Hold,
    Relieve,
}

struct LimiterCore {
    in_flight: AtomicUsize,
    limit: AtomicUsize,
    floor: usize,
    ceiling: usize,
    relieve_step: usize,
}

pub struct HeavyLimiter {
    core: Arc<LimiterCore>,
}

pub struct HeavyPermit {
    core: Arc<LimiterCore>,
}

impl Drop for HeavyPermit {
    fn drop(&mut self) {
        self.core.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

impl HeavyLimiter {
    pub fn new(max: MaxInFlight) -> Self {
        let ceiling = max.get();
        Self {
            core: Arc::new(LimiterCore {
                in_flight: AtomicUsize::new(0),
                limit: AtomicUsize::new(ceiling),
                floor: 1,
                ceiling,
                relieve_step: (ceiling / 16).max(1),
            }),
        }
    }

    pub fn try_enter(&self) -> Result<HeavyPermit, XrpcError> {
        let prev = self.core.in_flight.fetch_add(1, Ordering::AcqRel);
        if prev < self.core.limit.load(Ordering::Acquire) {
            Ok(HeavyPermit {
                core: Arc::clone(&self.core),
            })
        } else {
            self.core.in_flight.fetch_sub(1, Ordering::AcqRel);
            Err(XrpcError::overloaded())
        }
    }

    pub fn adjust(&self, verdict: PressureVerdict) {
        let cur = self.core.limit.load(Ordering::Acquire);
        let next = match verdict {
            PressureVerdict::Tighten => (cur / 2).max(self.core.floor),
            PressureVerdict::Relieve => (cur + self.core.relieve_step).min(self.core.ceiling),
            PressureVerdict::Hold => cur,
        };
        self.core.limit.store(next, Ordering::Release);
    }

    pub fn limit(&self) -> usize {
        self.core.limit.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_floor_above_budget_yields_one_slot() {
        let max = MaxInFlight::from_budget(
            MemoryBudget::new(100 * 1024 * 1024),
            ReservedFloor::new(114 * 1024 * 1024),
            PerRequestAnonBytes::new(2 * 1024 * 1024),
        );
        assert_eq!(max.get(), 1);
    }

    #[test]
    fn larger_budget_opens_more_slots() {
        let max = MaxInFlight::from_budget(
            MemoryBudget::new(500 * 1024 * 1024),
            ReservedFloor::new(114 * 1024 * 1024),
            PerRequestAnonBytes::new(2 * 1024 * 1024),
        );
        assert_eq!(max.get(), (386 * 1024 * 1024) / (2 * 1024 * 1024));
    }

    #[test]
    fn limiter_sheds_when_at_limit() {
        let limiter = HeavyLimiter::new(MaxInFlight(1));
        let held = limiter.try_enter().expect("first permit enters");
        assert!(matches!(limiter.try_enter(), Err(XrpcError::Overloaded)));
        drop(held);
        assert!(limiter.try_enter().is_ok());
    }

    #[test]
    fn adjust_clamps_between_floor_and_ceiling() {
        let limiter = HeavyLimiter::new(MaxInFlight(8));
        limiter.adjust(PressureVerdict::Tighten);
        assert_eq!(limiter.limit(), 4);
        limiter.adjust(PressureVerdict::Tighten);
        assert_eq!(limiter.limit(), 2);
        limiter.adjust(PressureVerdict::Tighten);
        assert_eq!(limiter.limit(), 1);
        limiter.adjust(PressureVerdict::Tighten);
        assert_eq!(limiter.limit(), 1);
        (0..20).for_each(|_| limiter.adjust(PressureVerdict::Relieve));
        assert_eq!(limiter.limit(), 8);
    }
}
