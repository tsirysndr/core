use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use knot_types::UnixMicros;

const MAX_TRACKED_PEERS: usize = 100_000;

const SWEEP_INTERVAL_MICROS: u64 = 1_000_000;

knot_types::scalar_newtype! {
    pub struct Burst(u32);
    pub struct RefillMicros(u64);
    pub struct PerPeerInflight(usize);
    pub struct GlobalInflight(usize);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    pub burst: Burst,
    pub refill: RefillMicros,
}

impl RateLimit {
    const fn interval(self) -> u64 {
        match self.refill.get() {
            0 => 1,
            micros => micros,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LimitConfig {
    pub rate: Option<RateLimit>,
    pub per_peer_inflight: Option<PerPeerInflight>,
    pub global_inflight: Option<GlobalInflight>,
}

impl Default for LimitConfig {
    fn default() -> Self {
        Self {
            rate: Some(RateLimit {
                burst: Burst::new(20),
                refill: RefillMicros::new(100_000),
            }),
            per_peer_inflight: Some(PerPeerInflight::new(8)),
            global_inflight: Some(GlobalInflight::new(64)),
        }
    }
}

impl LimitConfig {
    pub const fn per_peer_only(per_peer_inflight: PerPeerInflight) -> Self {
        Self {
            rate: None,
            per_peer_inflight: Some(per_peer_inflight),
            global_inflight: None,
        }
    }

    pub const fn unmetered() -> Self {
        Self {
            rate: None,
            per_peer_inflight: None,
            global_inflight: None,
        }
    }
}

struct Bucket {
    tokens: u32,
    last_refill: UnixMicros,
}

impl Bucket {
    fn new(rate: RateLimit, now: UnixMicros) -> Self {
        Self {
            tokens: rate.burst.get(),
            last_refill: now,
        }
    }

    fn tokens_at(&self, rate: RateLimit, now: UnixMicros) -> u32 {
        let elapsed = now.get().saturating_sub(self.last_refill.get());
        let gained = (elapsed / rate.interval()).min(u64::from(rate.burst.get())) as u32;
        self.tokens.saturating_add(gained).min(rate.burst.get())
    }

    fn replenish(&mut self, rate: RateLimit, now: UnixMicros) -> bool {
        if now.get().saturating_sub(self.last_refill.get()) >= rate.interval() {
            self.tokens = self.tokens_at(rate, now);
            self.last_refill = now;
        }
        self.tokens > 0
    }

    fn full(&self, rate: RateLimit, now: UnixMicros) -> bool {
        self.tokens_at(rate, now) >= rate.burst.get()
    }
}

struct PeerState {
    bucket: Option<Bucket>,
    inflight: usize,
}

impl PeerState {
    fn forgettable(&self) -> bool {
        self.inflight == 0 && self.bucket.is_none()
    }

    fn worth_tracking(&self, rate: Option<RateLimit>, now: UnixMicros) -> bool {
        match (self.inflight, &self.bucket, rate) {
            (0, Some(bucket), Some(rate)) => !bucket.full(rate, now),
            (0, _, _) => false,
            _ => true,
        }
    }
}

struct Inner {
    peers: HashMap<Option<IpAddr>, PeerState>,
    global_inflight: usize,
    last_sweep: UnixMicros,
}

pub struct PreAuthLimiter {
    config: LimitConfig,
    inner: Mutex<Inner>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    RateLimited,
    Saturated,
}

impl Default for PreAuthLimiter {
    fn default() -> Self {
        Self::with_config(LimitConfig::default())
    }
}

impl PreAuthLimiter {
    pub fn with_config(config: LimitConfig) -> Self {
        Self {
            config,
            inner: Mutex::new(Inner {
                peers: HashMap::new(),
                global_inflight: 0,
                last_sweep: UnixMicros::new(0),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn admit(
        self: &Arc<Self>,
        peer: Option<IpAddr>,
        now: UnixMicros,
    ) -> Result<AdmitGuard, Refusal> {
        let mut inner = self.lock();
        let rate = self.config.rate;
        let over_global = self
            .config
            .global_inflight
            .is_some_and(|limit| inner.global_inflight >= limit.get());

        // Only a rate budget outlives the work it admitted, so only a rate
        // budget can pile up entries for peers that have gone away. Without one
        // the last operation removes the entry and the table is already bounded
        // by live concurrency, so capping it would shed callers over a table
        // that can't grow.
        if rate.is_some()
            && !inner.peers.contains_key(&peer)
            && inner.peers.len() >= MAX_TRACKED_PEERS
        {
            let sweep_due =
                now.get().saturating_sub(inner.last_sweep.get()) >= SWEEP_INTERVAL_MICROS;
            if sweep_due {
                inner.last_sweep = now;
                inner
                    .peers
                    .retain(|_, state| state.worth_tracking(rate, now));
            }
            if inner.peers.len() >= MAX_TRACKED_PEERS {
                return Err(Refusal::Saturated);
            }
        }

        let per_peer = self.config.per_peer_inflight;
        let decision = {
            let state = inner.peers.entry(peer).or_insert_with(|| PeerState {
                bucket: rate.map(|rate| Bucket::new(rate, now)),
                inflight: 0,
            });
            let ready = match (rate, state.bucket.as_mut()) {
                (Some(rate), Some(bucket)) => bucket.replenish(rate, now),
                _ => true,
            };
            let over_peer = per_peer.is_some_and(|limit| state.inflight >= limit.get());
            match (ready, over_peer || over_global) {
                (false, _) => Err(Refusal::RateLimited),
                (_, true) => Err(Refusal::Saturated),
                (true, false) => {
                    if let Some(bucket) = state.bucket.as_mut() {
                        bucket.tokens -= 1;
                    }
                    state.inflight += 1;
                    Ok(())
                }
            }
        };
        match decision {
            Err(refusal) => {
                if inner.peers.get(&peer).is_some_and(PeerState::forgettable) {
                    inner.peers.remove(&peer);
                }
                Err(refusal)
            }
            Ok(()) => {
                inner.global_inflight += 1;
                Ok(AdmitGuard {
                    limiter: Arc::clone(self),
                    peer,
                })
            }
        }
    }

    fn leave(&self, peer: Option<IpAddr>) {
        let mut inner = self.lock();
        inner.global_inflight = inner.global_inflight.saturating_sub(1);
        let Some(state) = inner.peers.get_mut(&peer) else {
            return;
        };
        state.inflight = state.inflight.saturating_sub(1);
        if state.forgettable() {
            inner.peers.remove(&peer);
        }
    }

    fn repay(&self, peer: Option<IpAddr>) {
        let Some(rate) = self.config.rate else {
            return;
        };
        let mut inner = self.lock();
        if let Some(bucket) = inner
            .peers
            .get_mut(&peer)
            .and_then(|state| state.bucket.as_mut())
        {
            bucket.tokens = bucket.tokens.saturating_add(1).min(rate.burst.get());
        }
    }
}

pub struct AdmitGuard {
    limiter: Arc<PreAuthLimiter>,
    peer: Option<IpAddr>,
}

impl AdmitGuard {
    pub fn refund(self) {
        self.limiter.repay(self.peer);
    }
}

impl Drop for AdmitGuard {
    fn drop(&mut self) {
        self.limiter.leave(self.peer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn limiter(config: LimitConfig) -> Arc<PreAuthLimiter> {
        Arc::new(PreAuthLimiter::with_config(config))
    }

    fn rate(burst: u32, refill_micros: u64) -> Option<RateLimit> {
        Some(RateLimit {
            burst: Burst::new(burst),
            refill: RefillMicros::new(refill_micros),
        })
    }

    fn peer(last: u8) -> Option<IpAddr> {
        Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, last)))
    }

    fn rotating(index: u64) -> Option<IpAddr> {
        let octets = (index as u32).to_be_bytes();
        Some(IpAddr::V4(Ipv4Addr::new(
            1, octets[1], octets[2], octets[3],
        )))
    }

    fn at(micros: u64) -> UnixMicros {
        UnixMicros::new(micros)
    }

    fn tracked(limiter: &Arc<PreAuthLimiter>) -> usize {
        limiter.lock().peers.len()
    }

    #[test]
    fn a_rate_budget_refuses_a_flood_refills_over_time_and_stays_per_peer() {
        let limiter = limiter(LimitConfig {
            rate: rate(2, 1_000),
            per_peer_inflight: Some(PerPeerInflight::new(100)),
            global_inflight: Some(GlobalInflight::new(100)),
        });
        assert!(limiter.admit(peer(1), at(0)).is_ok());
        assert!(limiter.admit(peer(1), at(0)).is_ok());
        assert_eq!(
            limiter.admit(peer(1), at(0)).err(),
            Some(Refusal::RateLimited),
            "the limiter refuses a third request inside the same instant even when nothing is in flight"
        );
        assert!(
            limiter.admit(peer(2), at(0)).is_ok(),
            "a second peer keeps its own rate budget"
        );
        assert_eq!(
            limiter.admit(peer(1), at(600)).err(),
            Some(Refusal::RateLimited)
        );
        assert!(
            limiter.admit(peer(1), at(1_000)).is_ok(),
            "a refusal partway through the interval mustn't reset the clock the refill measures from"
        );
    }

    #[test]
    fn inflight_shedding_frees_on_drop_and_spends_no_rate_budget() {
        let limiter = limiter(LimitConfig {
            rate: rate(4, 1_000_000),
            per_peer_inflight: Some(PerPeerInflight::new(1)),
            global_inflight: Some(GlobalInflight::new(2)),
        });
        let held = limiter
            .admit(peer(1), at(0))
            .expect("the limiter admits the first request");
        let other = limiter
            .admit(peer(2), at(0))
            .expect("a second peer fills the global budget");
        assert_eq!(
            limiter.admit(peer(1), at(0)).err(),
            Some(Refusal::Saturated),
            "the limiter sheds a second concurrent request from one peer"
        );
        assert_eq!(
            limiter.admit(peer(3), at(0)).err(),
            Some(Refusal::Saturated),
            "the limiter sheds a third peer once the global in-flight limit is reached"
        );
        drop(other);
        drop(
            limiter
                .admit(peer(3), at(0))
                .expect("freeing a global slot admits the peer that was shed"),
        );
        drop(held);
        (0..50).for_each(|_| {
            limiter
                .admit(peer(1), at(0))
                .expect("a refunded admission must leave the full budget available")
                .refund();
        });
        (0..3).for_each(|_| {
            limiter
                .admit(peer(1), at(0))
                .expect("a shed request mustn't spend the rate budget it never used");
        });
        assert_eq!(
            limiter.admit(peer(1), at(0)).err(),
            Some(Refusal::RateLimited),
            "a dropped guard without a refund keeps its token spent"
        );
    }

    #[test]
    fn a_budget_without_a_rate_never_rate_limits_and_keeps_no_idle_state() {
        let overflowing = MAX_TRACKED_PEERS as u64 + 1_000;

        let per_peer = limiter(LimitConfig::per_peer_only(PerPeerInflight::new(2)));
        (0..1_000).for_each(|_| {
            per_peer
                .admit(peer(1), at(0))
                .expect("a budget with no rate has nothing for a sequential flood to exhaust");
        });
        let concurrent: Vec<AdmitGuard> = (0..2)
            .map(|_| {
                per_peer
                    .admit(peer(1), at(0))
                    .expect("the limiter admits both concurrent operations from one peer")
            })
            .collect();
        assert_eq!(
            per_peer.admit(peer(1), at(0)).err(),
            Some(Refusal::Saturated),
            "only the per-peer count refuses a request in this budget"
        );
        drop(concurrent);
        let held: Vec<AdmitGuard> = (0..overflowing)
            .map(|index| {
                per_peer
                    .admit(rotating(index), at(0))
                    .expect("a budget that keeps no idle state has no table to overflow")
            })
            .collect();
        assert_eq!(tracked(&per_peer), held.len());
        drop(held);
        assert_eq!(
            tracked(&per_peer),
            0,
            "with no tokens to remember, an idle peer leaves no entry, \
             so address rotation mustn't fill the table and start shedding newcomers"
        );

        let global = limiter(LimitConfig {
            rate: None,
            per_peer_inflight: None,
            global_inflight: Some(GlobalInflight::new(1)),
        });
        let _saturating = global
            .admit(peer(1), at(0))
            .expect("the limiter admits the first peer");
        (0..overflowing).for_each(|index| {
            assert_eq!(
                global.admit(rotating(index), at(0)).err(),
                Some(Refusal::Saturated)
            );
        });
        assert_eq!(
            tracked(&global),
            1,
            "a refusal returns no guard, so an entry it left behind would never be freed, \
             and the sweep that bounds the table only reclaims idle rate state"
        );

        let unmetered = limiter(LimitConfig::unmetered());
        let guards: Vec<AdmitGuard> = (0..512)
            .map(|_| {
                unmetered
                    .admit(peer(1), at(0))
                    .expect("an unmetered budget admits every request from every peer")
            })
            .collect();
        drop(guards);
        assert_eq!(tracked(&unmetered), 0);
    }

    #[test]
    fn a_rate_budget_bounds_its_peer_map_and_sweeps_at_most_once_per_interval() {
        let limiter = limiter(LimitConfig {
            rate: rate(1, 1_000),
            per_peer_inflight: Some(PerPeerInflight::new(8)),
            global_inflight: Some(GlobalInflight::new(64)),
        });
        (0..MAX_TRACKED_PEERS as u64).for_each(|index| {
            let _ = limiter.admit(rotating(index), at(0));
        });
        assert_eq!(
            limiter
                .admit(peer(201), at(SWEEP_INTERVAL_MICROS - 1))
                .err(),
            Some(Refusal::Saturated),
            "inside the sweep interval a full map sheds unseen peers without rescanning"
        );
        assert!(
            limiter.admit(peer(202), at(SWEEP_INTERVAL_MICROS)).is_ok(),
            "once the interval elapses the sweep evicts replenished entries and admits the newcomer"
        );
        (0..(MAX_TRACKED_PEERS as u64 + 50_000)).for_each(|index| {
            let _ = limiter.admit(
                rotating(MAX_TRACKED_PEERS as u64 + index),
                at(SWEEP_INTERVAL_MICROS + index),
            );
        });
        let tracked = tracked(&limiter);
        assert!(
            tracked <= MAX_TRACKED_PEERS,
            "a flood of distinct source addresses mustn't grow the peer map past its limit, saw {tracked}"
        );
    }
}
