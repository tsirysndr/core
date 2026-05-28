use tokio::sync::watch;

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HydrantCursor(u64);

impl HydrantCursor {
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl From<u64> for HydrantCursor {
    fn from(id: u64) -> Self {
        Self(id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Coverage {
    Warming {
        events_processed: u64,
        last_cursor: HydrantCursor,
    },
    Ready {
        events_processed: u64,
        last_cursor: HydrantCursor,
    },
}

impl Default for Coverage {
    fn default() -> Self {
        Self::Warming {
            events_processed: 0,
            last_cursor: HydrantCursor::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PromotionSignal {
    pub rev_micros: Option<u64>,
    pub now_micros: u64,
    pub skew_micros: u64,
}

impl PromotionSignal {
    pub fn caught_up(self) -> bool {
        let Some(rev) = self.rev_micros else {
            return false;
        };
        self.now_micros.abs_diff(rev) <= self.skew_micros
    }
}

impl Coverage {
    pub const fn is_ready(self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    pub const fn events_processed(self) -> u64 {
        match self {
            Self::Warming {
                events_processed, ..
            }
            | Self::Ready {
                events_processed, ..
            } => events_processed,
        }
    }

    pub const fn last_cursor(self) -> HydrantCursor {
        match self {
            Self::Warming { last_cursor, .. } | Self::Ready { last_cursor, .. } => last_cursor,
        }
    }

    pub fn advance(self, cursor: HydrantCursor) -> Self {
        debug_assert!(
            cursor.raw() >= self.last_cursor().raw(),
            "hydrant cursor regressed: {} -> {}",
            self.last_cursor().raw(),
            cursor.raw(),
        );
        let processed = self.events_processed().saturating_add(1);
        match self {
            Self::Warming { .. } => Self::Warming {
                events_processed: processed,
                last_cursor: cursor,
            },
            Self::Ready { .. } => Self::Ready {
                events_processed: processed,
                last_cursor: cursor,
            },
        }
    }

    pub fn maybe_promote(self, signal: PromotionSignal) -> Self {
        match self {
            Self::Ready { .. } => self,
            Self::Warming {
                events_processed,
                last_cursor,
            } if signal.caught_up() => Self::Ready {
                events_processed,
                last_cursor,
            },
            warming => warming,
        }
    }

    pub const fn force_ready(self) -> Self {
        match self {
            Self::Ready { .. } => self,
            Self::Warming {
                events_processed,
                last_cursor,
            } => Self::Ready {
                events_processed,
                last_cursor,
            },
        }
    }
}

#[derive(Debug)]
pub struct CoverageWatch {
    tx: watch::Sender<Coverage>,
}

impl Default for CoverageWatch {
    fn default() -> Self {
        Self::new()
    }
}

impl CoverageWatch {
    pub fn new() -> Self {
        let (tx, _) = watch::channel(Coverage::default());
        Self { tx }
    }

    pub fn snapshot(&self) -> Coverage {
        *self.tx.borrow()
    }

    pub fn subscribe(&self) -> watch::Receiver<Coverage> {
        self.tx.subscribe()
    }

    pub fn update<F>(&self, transform: F)
    where
        F: FnOnce(Coverage) -> Coverage,
    {
        self.tx.send_if_modified(|c| {
            let next = transform(*c);
            if *c == next {
                false
            } else {
                *c = next;
                true
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SKEW: u64 = 60_000_000;

    fn signal(rev: u64, now: u64) -> PromotionSignal {
        PromotionSignal {
            rev_micros: Some(rev),
            now_micros: now,
            skew_micros: SKEW,
        }
    }

    #[test]
    fn defaults_to_warming_at_zero() {
        let c = Coverage::default();
        assert!(!c.is_ready());
        assert_eq!(c.events_processed(), 0);
        assert_eq!(c.last_cursor(), HydrantCursor::default());
    }

    #[test]
    fn advance_increments_and_tracks_cursor() {
        let c = Coverage::default()
            .advance(HydrantCursor::new(7))
            .advance(HydrantCursor::new(9));
        assert_eq!(c.events_processed(), 2);
        assert_eq!(c.last_cursor(), HydrantCursor::new(9));
        assert!(!c.is_ready());
    }

    #[test]
    fn promotion_requires_recent_rev() {
        let now = 1_000_000_000;
        let recent = now - SKEW / 2;
        let stale = now - SKEW * 10;

        let c = Coverage::default().advance(HydrantCursor::new(1));
        assert!(!c.maybe_promote(signal(stale, now)).is_ready());
        assert!(c.maybe_promote(signal(recent, now)).is_ready());
    }

    #[test]
    fn promotion_preserves_counters() {
        let now = 1_000_000_000;
        let c = Coverage::default()
            .advance(HydrantCursor::new(3))
            .maybe_promote(signal(now, now))
            .advance(HydrantCursor::new(4));
        assert!(c.is_ready());
        assert_eq!(c.events_processed(), 2);
        assert_eq!(c.last_cursor(), HydrantCursor::new(4));
    }

    #[test]
    fn ready_is_sticky() {
        let now = 1_000_000_000;
        let stale = now - SKEW * 10;
        let c = Coverage::default()
            .advance(HydrantCursor::new(1))
            .maybe_promote(signal(now, now))
            .advance(HydrantCursor::new(2))
            .maybe_promote(signal(stale, now));
        assert!(c.is_ready());
    }

    #[test]
    fn future_rev_within_skew_promotes() {
        let now = 1_000_000_000;
        let near_future = now + SKEW / 2;
        let c = Coverage::default()
            .advance(HydrantCursor::new(1))
            .maybe_promote(signal(near_future, now));
        assert!(c.is_ready());
    }

    #[test]
    fn future_rev_beyond_skew_does_not_promote() {
        let now = 1_000_000_000;
        let far_future = now + SKEW * 10;
        let c = Coverage::default()
            .advance(HydrantCursor::new(1))
            .maybe_promote(signal(far_future, now));
        assert!(!c.is_ready());
    }

    #[test]
    fn missing_rev_does_not_promote() {
        let c = Coverage::default().advance(HydrantCursor::new(1));
        let s = PromotionSignal {
            rev_micros: None,
            now_micros: 1_000_000_000,
            skew_micros: SKEW,
        };
        assert!(!c.maybe_promote(s).is_ready());
    }
}
