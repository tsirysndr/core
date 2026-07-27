use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use knot_cache::{Cache, EntryCount, Lru, Reclaimable, Weight};
use knot_runtime::Clock;
use tokio::sync::watch;

const MAX_ENTRIES: usize = 1024;

knot_types::scalar_newtype! {
    pub struct MaxEntryBytes(usize);
    pub struct MaxCacheBytes(usize);
}

#[derive(Debug, Clone, Copy)]
pub struct CacheConfig {
    pub enabled: bool,
    pub ttl: Duration,
    pub max_entry_bytes: MaxEntryBytes,
    pub max_total_bytes: MaxCacheBytes,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ttl: Duration::from_secs(60),
            max_entry_bytes: MaxEntryBytes::new(64 * 1024 * 1024),
            max_total_bytes: MaxCacheBytes::new(512 * 1024 * 1024),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct RequestKey {
    objects_dir: PathBuf,
    digest: [u8; 32],
}

impl RequestKey {
    pub(crate) fn new(objects_dir: &Path, ref_token: &crate::ids::RefsDigest, body: &[u8]) -> Self {
        let mut hasher = gix::hash::hasher(gix::hash::Kind::Sha256);
        hasher.update(ref_token.as_bytes());
        hasher.update(body);
        let id = hasher.try_finalize().expect("sha256 digest finalizes");
        let mut digest = [0u8; 32];
        digest.copy_from_slice(id.as_slice());
        Self {
            objects_dir: objects_dir.to_path_buf(),
            digest,
        }
    }
}

#[derive(Clone)]
pub(crate) enum Signal {
    Pending,
    Ready(Bytes),
    Retry,
    Regenerate,
}

#[derive(Clone)]
enum Slot {
    Ready(Bytes),
    TooLarge,
}

fn weigh(slot: &Slot) -> Weight {
    match slot {
        Slot::Ready(bytes) => Weight::new(bytes.len() as u64),
        Slot::TooLarge => Weight::new(0),
    }
}

enum Settlement {
    Ready(Bytes),
    TooLarge,
    Regenerate,
    Retry,
}

pub(crate) struct PackCache {
    config: CacheConfig,
    store: Lru<RequestKey, Slot, Arc<dyn Clock>>,
    inflight: Mutex<HashMap<RequestKey, watch::Sender<Signal>>>,
}

pub(crate) enum Decision {
    Serve(Bytes),
    Await(watch::Receiver<Signal>),
    Lead(Lease),
    Stream,
    Off,
}

impl PackCache {
    pub(crate) fn new(mut config: CacheConfig, clock: Arc<dyn Clock>) -> Arc<Self> {
        config.max_entry_bytes = MaxEntryBytes::new(
            config
                .max_entry_bytes
                .get()
                .min(config.max_total_bytes.get()),
        );
        let store = Lru::by_weight_with_ttl(
            Weight::new(config.max_total_bytes.get() as u64),
            config.ttl,
            clock,
            weigh,
        )
        .with_entry_cap(EntryCount::new(MAX_ENTRIES as u64));
        let cache = Arc::new(Self {
            config,
            store,
            inflight: Mutex::new(HashMap::new()),
        });
        knot_cache::register(&cache);
        cache
    }

    pub(crate) fn decide(self: &Arc<Self>, key: RequestKey) -> Decision {
        if !self.config.enabled {
            return Decision::Off;
        }
        if let Some(decision) = self.serve_cached(&key) {
            return decision;
        }
        let mut inflight = self.inflight_lock();
        if let Some(sender) = inflight.get(&key) {
            return Decision::Await(sender.subscribe());
        }
        // `store` & `inflight` are separate locks,
        // such that a leader can finish settling in the gap between
        // a fast-path miss and this lock acquisition,
        // by which point it has inserted the pack and taken its sender away.
        // Reading the store a second time will cover that gap,
        // since `settle` always inserts before it removes.
        //
        // Without it,
        // the race-losing caller would elect itself and rebuild a pack
        // the store does in fact already have.
        if let Some(decision) = self.serve_cached(&key) {
            return decision;
        }
        let (sender, _) = watch::channel(Signal::Pending);
        inflight.insert(key.clone(), sender);
        Decision::Lead(Lease {
            key,
            cache: Arc::clone(self),
            max_entry_bytes: self.config.max_entry_bytes,
            settled: false,
        })
    }

    fn serve_cached(&self, key: &RequestKey) -> Option<Decision> {
        match self.store.get(key) {
            Some(Slot::Ready(bytes)) => Some(Decision::Serve(bytes)),
            Some(Slot::TooLarge) => Some(Decision::Stream),
            None => None,
        }
    }

    fn settle(&self, key: &RequestKey, settlement: Settlement) {
        let signal = match settlement {
            Settlement::Ready(bytes) => {
                self.store.insert(key.clone(), Slot::Ready(bytes.clone()));
                Signal::Ready(bytes)
            }
            Settlement::TooLarge => {
                self.store.insert(key.clone(), Slot::TooLarge);
                Signal::Regenerate
            }
            Settlement::Regenerate => Signal::Regenerate,
            Settlement::Retry => Signal::Retry,
        };
        if let Some(sender) = self.inflight_lock().remove(key) {
            let _ = sender.send(signal);
        }
    }

    fn inflight_lock(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<RequestKey, watch::Sender<Signal>>> {
        self.inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    fn entry_count(&self) -> u64 {
        self.store.entry_count().get()
    }
}

impl Reclaimable for PackCache {
    fn footprint(&self) -> Weight {
        self.store.footprint()
    }

    fn reclaim(&self) {
        self.store.reclaim();
    }
}

pub(crate) struct Lease {
    key: RequestKey,
    cache: Arc<PackCache>,
    max_entry_bytes: MaxEntryBytes,
    settled: bool,
}

impl Lease {
    pub(crate) fn max_entry_bytes(&self) -> MaxEntryBytes {
        self.max_entry_bytes
    }

    pub(crate) fn ready(mut self, bytes: Bytes) {
        self.cache.settle(&self.key, Settlement::Ready(bytes));
        self.settled = true;
    }

    pub(crate) fn too_large(mut self) {
        self.cache.settle(&self.key, Settlement::TooLarge);
        self.settled = true;
    }

    pub(crate) fn regenerate(mut self) {
        self.cache.settle(&self.key, Settlement::Regenerate);
        self.settled = true;
    }

    pub(crate) fn retry(mut self) {
        self.cache.settle(&self.key, Settlement::Retry);
        self.settled = true;
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if !self.settled {
            self.cache.settle(&self.key, Settlement::Retry);
        }
    }
}

pub(crate) enum Resolved {
    Bytes(Bytes),
    Retry,
    Regenerate,
}

pub(crate) async fn wait(mut receiver: watch::Receiver<Signal>) -> Resolved {
    let resolved = classify(&receiver.borrow_and_update());
    match resolved {
        Some(resolved) => resolved,
        None => match receiver.changed().await {
            Err(_) => Resolved::Regenerate,
            Ok(()) => Box::pin(wait(receiver)).await,
        },
    }
}

fn classify(signal: &Signal) -> Option<Resolved> {
    match signal {
        Signal::Pending => None,
        Signal::Ready(bytes) => Some(Resolved::Bytes(bytes.clone())),
        Signal::Retry => Some(Resolved::Retry),
        Signal::Regenerate => Some(Resolved::Regenerate),
    }
}

pub(crate) enum Capture {
    Buffering { buffer: Vec<u8>, limit: usize },
    Overflow,
    Off,
}

impl Capture {
    pub(crate) fn new(limit: Option<MaxEntryBytes>) -> Self {
        match limit {
            Some(limit) => Capture::Buffering {
                buffer: Vec::new(),
                limit: limit.get(),
            },
            None => Capture::Off,
        }
    }

    pub(crate) fn record(&mut self, chunk: &[u8]) {
        match self {
            Capture::Buffering { buffer, limit } if buffer.len() + chunk.len() <= *limit => {
                buffer.extend_from_slice(chunk)
            }
            Capture::Buffering { .. } => *self = Capture::Overflow,
            _ => {}
        }
    }

    pub(crate) fn into_bytes(self) -> Option<Vec<u8>> {
        match self {
            Capture::Buffering { buffer, .. } => Some(buffer),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use knot_runtime::{ManualClock, SystemClock, UnixMicros};

    use super::*;

    fn built(config: CacheConfig) -> Arc<PackCache> {
        PackCache::new(config, Arc::new(SystemClock))
    }

    fn key(body: &[u8]) -> RequestKey {
        let mut token = [0u8; 32];
        token[..4].copy_from_slice(b"refs");
        RequestKey::new(
            Path::new("/scan/did:plc:squid/objects"),
            &crate::ids::RefsDigest::new(token),
            body,
        )
    }

    fn lead(cache: &Arc<PackCache>, body: &[u8]) -> Lease {
        match cache.decide(key(body)) {
            Decision::Lead(lease) => lease,
            _ => panic!("the first request for a key leads"),
        }
    }

    #[tokio::test]
    async fn a_second_identical_request_serves_the_cached_pack() {
        let cache = built(CacheConfig::default());
        lead(&cache, b"want").ready(Bytes::from_static(b"PACK-bytes"));
        match cache.decide(key(b"want")) {
            Decision::Serve(bytes) => assert_eq!(bytes.as_ref(), b"PACK-bytes"),
            _ => panic!("second identical request hits the cache"),
        }
    }

    #[tokio::test]
    async fn a_concurrent_request_awaits_the_leader_then_shares_its_bytes() {
        let cache = built(CacheConfig::default());
        let leader = lead(&cache, b"clone");
        let receiver = match cache.decide(key(b"clone")) {
            Decision::Await(receiver) => receiver,
            _ => panic!("concurrent request awaits the inflight leader"),
        };
        leader.ready(Bytes::from_static(b"shared"));
        match wait(receiver).await {
            Resolved::Bytes(bytes) => assert_eq!(bytes.as_ref(), b"shared"),
            _ => panic!("the follower shares the leader's bytes"),
        }
    }

    #[tokio::test]
    async fn a_regenerating_leader_wakes_followers_to_regenerate() {
        let cache = built(CacheConfig::default());
        let leader = lead(&cache, b"err");
        let receiver = match cache.decide(key(b"err")) {
            Decision::Await(receiver) => receiver,
            _ => panic!("follower awaits"),
        };
        leader.regenerate();
        assert!(
            matches!(wait(receiver).await, Resolved::Regenerate),
            "an errored leader tells the follower to regenerate in parallel"
        );
        match cache.decide(key(b"err")) {
            Decision::Lead(_) => {}
            _ => panic!("a regenerated key isn't remembered, so the next request leads afresh"),
        }
    }

    #[tokio::test]
    async fn a_retrying_leader_tells_followers_to_re_elect() {
        let cache = built(CacheConfig::default());
        let leader = lead(&cache, b"vanish");
        let receiver = match cache.decide(key(b"vanish")) {
            Decision::Await(receiver) => receiver,
            _ => panic!("follower awaits"),
        };
        leader.retry();
        assert!(
            matches!(wait(receiver).await, Resolved::Retry),
            "a vanished leader tells the follower to re-elect a fresh leader"
        );
        match cache.decide(key(b"vanish")) {
            Decision::Lead(_) => {}
            _ => panic!("a retried key isn't remembered, so the next request leads afresh"),
        }
    }

    #[tokio::test]
    async fn a_dropped_lease_re_elects_rather_than_stranding_the_follower() {
        let cache = built(CacheConfig::default());
        let leader = lead(&cache, b"dropped");
        let receiver = match cache.decide(key(b"dropped")) {
            Decision::Await(receiver) => receiver,
            _ => panic!("follower awaits"),
        };
        drop(leader);
        assert!(
            matches!(wait(receiver).await, Resolved::Retry),
            "an unsettled lease that drops re-elects instead of stranding the follower"
        );
    }

    #[tokio::test]
    async fn an_oversized_leader_marks_the_key_for_direct_streaming() {
        let cache = built(CacheConfig::default());
        let leader = lead(&cache, b"huge");
        let receiver = match cache.decide(key(b"huge")) {
            Decision::Await(receiver) => receiver,
            _ => panic!("follower awaits"),
        };
        leader.too_large();
        assert!(
            matches!(wait(receiver).await, Resolved::Regenerate),
            "an oversized leader sends its followers to stream directly"
        );
        match cache.decide(key(b"huge")) {
            Decision::Stream => {}
            _ => panic!("an oversized key streams directly without re-buffering"),
        }
    }

    #[tokio::test]
    async fn an_expired_entry_is_regenerated() {
        let clock = Arc::new(ManualClock::new(UnixMicros::new(0)));
        let cache = PackCache::new(
            CacheConfig {
                ttl: Duration::from_secs(60),
                ..CacheConfig::default()
            },
            Arc::clone(&clock) as Arc<dyn Clock>,
        );
        lead(&cache, b"stale").ready(Bytes::from_static(b"old"));
        clock.advance(Duration::from_secs(61));
        match cache.decide(key(b"stale")) {
            Decision::Lead(_) => {}
            _ => panic!("an expired entry forces a fresh generation"),
        }
    }

    #[tokio::test]
    async fn the_total_byte_limit_evicts_the_oldest_entry() {
        let cache = built(CacheConfig {
            max_total_bytes: MaxCacheBytes::new(8),
            ..CacheConfig::default()
        });
        lead(&cache, b"a").ready(Bytes::from(vec![0u8; 5]));
        lead(&cache, b"b").ready(Bytes::from(vec![0u8; 5]));
        match cache.decide(key(b"a")) {
            Decision::Lead(_) => {}
            _ => panic!("the oldest entry is evicted once the total limit is exceeded"),
        }
        match cache.decide(key(b"b")) {
            Decision::Serve(_) => {}
            _ => panic!("the newest entry survives eviction"),
        }
    }

    #[tokio::test]
    async fn the_entry_limit_never_exceeds_the_total_cache_size() {
        let cache = built(CacheConfig {
            max_entry_bytes: MaxEntryBytes::new(64),
            max_total_bytes: MaxCacheBytes::new(16),
            ..CacheConfig::default()
        });
        let lease = lead(&cache, b"probe");
        assert_eq!(
            lease.max_entry_bytes().get(),
            16,
            "a per-entry limit above the whole-cache size is clamped so a full entry can be retained"
        );
    }

    #[tokio::test]
    async fn oversized_entries_cannot_grow_the_cache_without_bound() {
        let cache = built(CacheConfig::default());
        (0..(MAX_ENTRIES + 64)).for_each(|nonce| {
            lead(&cache, format!("oversized-{nonce}").as_bytes()).too_large();
        });
        assert!(
            cache.entry_count() <= MAX_ENTRIES as u64,
            "a flood of distinct oversized requests stays within the entry bound"
        );
    }

    #[test]
    fn capture_stops_buffering_once_the_limit_is_passed() {
        let mut capture = Capture::new(Some(MaxEntryBytes::new(4)));
        capture.record(b"abc");
        capture.record(b"de");
        assert!(capture.into_bytes().is_none(), "overflow drops the buffer");
    }

    #[test]
    fn capture_keeps_bytes_under_the_limit() {
        let mut capture = Capture::new(Some(MaxEntryBytes::new(8)));
        capture.record(b"abcd");
        assert_eq!(capture.into_bytes().unwrap(), b"abcd");
    }
}
