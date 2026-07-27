mod expiring;

pub use expiring::{Admitted, Expiring, GroupQuota, Quotas, Rejected, TotalQuota};

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use knot_runtime::{Clock, UnixMicros};

knot_types::scalar_newtype! {
    pub struct EntryCount(u64);
    pub struct Weight(u64);
}

pub trait Cache<K, V>: Send + Sync {
    fn get(&self, key: &K) -> Option<V>;
    fn insert(&self, key: K, value: V);
    fn invalidate(&self, key: &K);
    fn invalidate_all(&self);
    fn entry_count(&self) -> EntryCount;
    fn weighted_size(&self) -> Weight;
}

pub struct Untimed;

impl Clock for Untimed {
    fn now_unix_micros(&self) -> UnixMicros {
        UnixMicros::new(0)
    }
}

pub struct Moka<K, V> {
    inner: moka::sync::Cache<K, V>,
}

impl<K, V> Moka<K, V>
where
    K: Hash + Eq + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    pub fn by_count(max_entries: EntryCount) -> Self {
        Self {
            inner: moka::sync::Cache::builder()
                .max_capacity(max_entries.get())
                .build(),
        }
    }

    pub fn by_weight<F>(max_weight: Weight, weigh: F) -> Self
    where
        F: Fn(&V) -> Weight + Send + Sync + 'static,
    {
        Self {
            inner: moka::sync::Cache::builder()
                .max_capacity(max_weight.get())
                .weigher(move |_key: &K, value: &V| weigh(value).get().min(u32::MAX as u64) as u32)
                .build(),
        }
    }

    pub fn get_or_try_insert_with<E, F>(&self, key: K, init: F) -> Result<V, Arc<E>>
    where
        F: FnOnce() -> Result<V, E>,
        E: Send + Sync + 'static,
    {
        self.inner.try_get_with(key, init)
    }
}

impl<K, V> Cache<K, V> for Moka<K, V>
where
    K: Hash + Eq + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    fn get(&self, key: &K) -> Option<V> {
        self.inner.get(key)
    }

    fn insert(&self, key: K, value: V) {
        self.inner.insert(key, value);
    }

    fn invalidate(&self, key: &K) {
        self.inner.invalidate(key);
    }

    fn invalidate_all(&self) {
        self.inner.invalidate_all();
    }

    fn entry_count(&self) -> EntryCount {
        EntryCount::new(self.inner.entry_count())
    }

    fn weighted_size(&self) -> Weight {
        Weight::new(self.inner.weighted_size())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Tick(u64);

impl Tick {
    fn issue(&mut self) -> Tick {
        let issued = *self;
        self.0 = self.0.saturating_add(1);
        issued
    }
}

struct Node<V> {
    value: V,
    tick: Tick,
    weight: u64,
    expires_at: Option<UnixMicros>,
}

struct LruInner<K, V> {
    by_key: HashMap<K, Node<V>>,
    order: BTreeMap<Tick, K>,
    next: Tick,
    total_weight: u64,
}

type Weigh<V> = Arc<dyn Fn(&V) -> Weight + Send + Sync>;

pub struct Lru<K, V, C: Clock = Untimed> {
    inner: Mutex<LruInner<K, V>>,
    max_entries: Option<EntryCount>,
    max_weight: Option<Weight>,
    weigh: Option<Weigh<V>>,
    ttl: Option<Duration>,
    clock: C,
}

impl<K, V> Lru<K, V, Untimed>
where
    K: Hash + Eq + Clone,
    V: Clone,
{
    pub fn by_count(max_entries: EntryCount) -> Self {
        Self::build(Some(max_entries), None, None, None, Untimed)
    }

    pub fn by_weight<F>(max_weight: Weight, weigh: F) -> Self
    where
        F: Fn(&V) -> Weight + Send + Sync + 'static,
    {
        Self::build(None, Some(max_weight), Some(Arc::new(weigh)), None, Untimed)
    }
}

impl<K, V, C> Lru<K, V, C>
where
    K: Hash + Eq + Clone,
    V: Clone,
    C: Clock,
{
    pub fn by_count_with_ttl(max_entries: EntryCount, ttl: Duration, clock: C) -> Self {
        Self::build(Some(max_entries), None, None, Some(ttl), clock)
    }

    pub fn by_weight_with_ttl<F>(max_weight: Weight, ttl: Duration, clock: C, weigh: F) -> Self
    where
        F: Fn(&V) -> Weight + Send + Sync + 'static,
    {
        Self::build(
            None,
            Some(max_weight),
            Some(Arc::new(weigh)),
            Some(ttl),
            clock,
        )
    }

    pub fn with_entry_cap(mut self, max_entries: EntryCount) -> Self {
        self.max_entries = Some(max_entries);
        self
    }

    fn build(
        max_entries: Option<EntryCount>,
        max_weight: Option<Weight>,
        weigh: Option<Weigh<V>>,
        ttl: Option<Duration>,
        clock: C,
    ) -> Self {
        Self {
            inner: Mutex::new(LruInner {
                by_key: HashMap::new(),
                order: BTreeMap::new(),
                next: Tick(0),
                total_weight: 0,
            }),
            max_entries,
            max_weight,
            weigh,
            ttl,
            clock,
        }
    }

    fn weight_of(&self, value: &V) -> u64 {
        self.weigh.as_ref().map_or(1, |weigh| weigh(value).get())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LruInner<K, V>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl<K, V, C> Cache<K, V> for Lru<K, V, C>
where
    K: Hash + Eq + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    C: Clock,
{
    fn get(&self, key: &K) -> Option<V> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let (stale, expires_at) = inner
            .by_key
            .get(key)
            .map(|node| (node.tick, node.expires_at))?;
        if expires_at.is_some_and(|at| at.get() <= self.clock.now_unix_micros().get()) {
            if let Some(node) = inner.by_key.remove(key) {
                inner.total_weight = inner.total_weight.saturating_sub(node.weight);
            }
            inner.order.remove(&stale);
            return None;
        }
        let fresh = inner.next.issue();
        inner.order.remove(&stale);
        inner.order.insert(fresh, key.clone());
        let node = inner.by_key.get_mut(key).expect("hit is still present");
        node.tick = fresh;
        Some(node.value.clone())
    }

    fn insert(&self, key: K, value: V) {
        let expires_at = self.ttl.map(|ttl| {
            UnixMicros::new(
                self.clock
                    .now_unix_micros()
                    .get()
                    .saturating_add(ttl.as_micros() as u64),
            )
        });
        let weight = self.weight_of(&value);
        let mut guard = self.lock();
        let inner = &mut *guard;
        if let Some(previous) = inner.by_key.remove(&key) {
            inner.order.remove(&previous.tick);
            inner.total_weight = inner.total_weight.saturating_sub(previous.weight);
        }
        let fresh = inner.next.issue();
        inner.order.insert(fresh, key.clone());
        inner.total_weight = inner.total_weight.saturating_add(weight);
        inner.by_key.insert(
            key,
            Node {
                value,
                tick: fresh,
                weight,
                expires_at,
            },
        );
        while self
            .max_entries
            .is_some_and(|max| inner.by_key.len() as u64 > max.get())
            || self
                .max_weight
                .is_some_and(|max| inner.total_weight > max.get())
        {
            let Some((_, evicted)) = inner.order.pop_first() else {
                break;
            };
            if let Some(node) = inner.by_key.remove(&evicted) {
                inner.total_weight = inner.total_weight.saturating_sub(node.weight);
            }
        }
    }

    fn invalidate(&self, key: &K) {
        let mut guard = self.lock();
        if let Some(node) = guard.by_key.remove(key) {
            guard.order.remove(&node.tick);
            guard.total_weight = guard.total_weight.saturating_sub(node.weight);
        }
    }

    fn invalidate_all(&self) {
        let mut guard = self.lock();
        guard.by_key.clear();
        guard.order.clear();
        guard.total_weight = 0;
    }

    fn entry_count(&self) -> EntryCount {
        EntryCount::new(self.lock().by_key.len() as u64)
    }

    fn weighted_size(&self) -> Weight {
        Weight::new(self.lock().total_weight)
    }
}

pub struct Noop;

impl<K, V> Cache<K, V> for Noop
where
    K: Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    fn get(&self, _key: &K) -> Option<V> {
        None
    }

    fn insert(&self, _key: K, _value: V) {}

    fn invalidate(&self, _key: &K) {}

    fn invalidate_all(&self) {}

    fn entry_count(&self) -> EntryCount {
        EntryCount::new(0)
    }

    fn weighted_size(&self) -> Weight {
        Weight::new(0)
    }
}

pub struct Filled<V> {
    pub value: V,
    pub fresh: bool,
}

type DeterministicHasher = std::hash::BuildHasherDefault<std::collections::hash_map::DefaultHasher>;

pub trait AsyncCache<K, V>: Send + Sync {
    fn get(&self, key: &K) -> impl Future<Output = Option<V>> + Send;
    fn entry_count(&self) -> EntryCount;
}

pub struct MokaFuture<K, V> {
    inner: moka::future::Cache<K, V, DeterministicHasher>,
}

impl<K, V> MokaFuture<K, V>
where
    K: Hash + Eq + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    pub fn by_count(max_entries: EntryCount) -> Self {
        Self {
            inner: moka::future::Cache::builder()
                .max_capacity(max_entries.get())
                .build_with_hasher(DeterministicHasher::default()),
        }
    }

    pub async fn get_or_fill_if<Fut, P>(&self, key: K, refill_if: P, fill: Fut) -> Filled<V>
    where
        Fut: Future<Output = V> + Send,
        P: FnMut(&V) -> bool + Send,
    {
        let entry = self
            .inner
            .entry(key)
            .or_insert_with_if(fill, refill_if)
            .await;
        Filled {
            fresh: entry.is_fresh(),
            value: entry.into_value(),
        }
    }

    pub async fn run_pending_tasks(&self) {
        self.inner.run_pending_tasks().await;
    }
}

impl<K, V> AsyncCache<K, V> for MokaFuture<K, V>
where
    K: Hash + Eq + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    fn get(&self, key: &K) -> impl Future<Output = Option<V>> + Send {
        self.inner.get(key)
    }

    fn entry_count(&self) -> EntryCount {
        EntryCount::new(self.inner.entry_count())
    }
}

pub trait Reclaimable: Send + Sync {
    fn footprint(&self) -> Weight;
    fn reclaim(&self);
}

impl<K, V> Reclaimable for Moka<K, V>
where
    K: Hash + Eq + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    fn footprint(&self) -> Weight {
        self.weighted_size()
    }

    fn reclaim(&self) {
        self.invalidate_all();
    }
}

impl<K, V, C> Reclaimable for Lru<K, V, C>
where
    K: Hash + Eq + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    C: Clock,
{
    fn footprint(&self) -> Weight {
        self.weighted_size()
    }

    fn reclaim(&self) {
        self.invalidate_all();
    }
}

#[derive(Default)]
struct Registry {
    caches: Mutex<Vec<Weak<dyn Reclaimable>>>,
}

impl Registry {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Weak<dyn Reclaimable>>> {
        self.caches
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn register<T: Reclaimable + 'static>(&self, cache: &Arc<T>) {
        let erased: Arc<dyn Reclaimable> = cache.clone();
        let weak = Arc::downgrade(&erased);
        let mut caches = self.lock();
        caches.retain(|entry| entry.strong_count() > 0);
        caches.push(weak);
    }

    fn reclaim_largest(&self) -> Option<Weight> {
        let largest = self
            .lock()
            .iter()
            .filter_map(Weak::upgrade)
            .max_by_key(|cache| cache.footprint().get())?;
        let freed = largest.footprint();
        (freed.get() > 0).then(|| {
            largest.reclaim();
            freed
        })
    }
}

fn global_registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Registry::default)
}

pub fn register<T: Reclaimable + 'static>(cache: &Arc<T>) {
    global_registry().register(cache);
}

pub fn reclaim_largest() -> Option<Weight> {
    global_registry().reclaim_largest()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use knot_runtime::ManualClock;

    use super::*;

    #[test]
    fn a_no_ttl_cache_retains_until_invalidated() {
        let cache: Moka<u32, u32> = Moka::by_count(EntryCount::new(16));
        cache.insert(1, 9);
        cache.insert(2, 10);
        assert_eq!(cache.get(&1), Some(9));
        cache.invalidate(&1);
        assert_eq!(cache.get(&1), None);
        assert_eq!(cache.get(&2), Some(10));
    }

    #[test]
    fn the_same_clock_sequence_yields_the_same_hits_and_misses() {
        let run = || {
            let clock = Arc::new(ManualClock::new(UnixMicros::new(0)));
            let cache: Lru<u32, u32, _> = Lru::by_count_with_ttl(
                EntryCount::new(16),
                Duration::from_secs(5),
                Arc::clone(&clock),
            );
            cache.insert(1, 100);
            let before = cache.get(&1);
            clock.advance(Duration::from_secs(6));
            let after = cache.get(&1);
            (before, after)
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn get_or_try_insert_with_serves_the_first_value_without_recomputing() {
        let cache: Moka<u32, u32> = Moka::by_count(EntryCount::new(16));
        let first = cache.get_or_try_insert_with(1, || Ok::<u32, ()>(7));
        assert_eq!(first.unwrap(), 7);
        let second = cache.get_or_try_insert_with(1, || Ok::<u32, ()>(99));
        assert_eq!(second.unwrap(), 7);
    }

    #[test]
    fn get_or_try_insert_with_propagates_the_error_and_stores_nothing() {
        let cache: Moka<u32, u32> = Moka::by_count(EntryCount::new(16));
        let failed = cache.get_or_try_insert_with(1, || Err::<u32, u32>(9));
        assert_eq!(*failed.unwrap_err(), 9);
        assert_eq!(cache.get(&1), None);
    }

    #[test]
    fn the_lru_evicts_the_least_recently_used_key() {
        let cache: Lru<u32, u32> = Lru::by_count(EntryCount::new(3));
        cache.insert(0, 0);
        cache.insert(1, 1);
        cache.insert(2, 2);
        assert_eq!(cache.get(&0), Some(0));
        cache.insert(3, 3);
        assert_eq!(
            cache.get(&1),
            None,
            "the least recently used key is evicted"
        );
        assert_eq!(cache.get(&0), Some(0), "the touched key survives");
        assert_eq!(cache.get(&3), Some(3));
        assert_eq!(cache.entry_count().get(), 3);
    }

    #[test]
    fn the_weighted_lru_evicts_oldest_until_it_fits_the_byte_budget() {
        let cache: Lru<u32, Vec<u8>> = Lru::by_weight(Weight::new(8), |value: &Vec<u8>| {
            Weight::new(value.len() as u64)
        });
        cache.insert(0, vec![0u8; 5]);
        cache.insert(1, vec![0u8; 5]);
        assert_eq!(cache.get(&0), None, "the oldest entry is evicted to fit");
        assert_eq!(cache.get(&1), Some(vec![0u8; 5]));
        assert_eq!(cache.weighted_size().get(), 5);
    }

    #[test]
    fn the_entry_cap_bounds_a_flood_of_zero_weight_entries() {
        let cache: Lru<u32, Vec<u8>> = Lru::by_weight(Weight::new(1_000_000), |value: &Vec<u8>| {
            Weight::new(value.len() as u64)
        })
        .with_entry_cap(EntryCount::new(4));
        (0..64).for_each(|nonce| cache.insert(nonce, Vec::new()));
        assert!(cache.entry_count().get() <= 4);
    }

    #[test]
    fn the_lru_expires_entries_on_the_injected_clock() {
        let clock = Arc::new(ManualClock::new(UnixMicros::new(0)));
        let cache: Lru<u32, u32, _> = Lru::by_count_with_ttl(
            EntryCount::new(8),
            Duration::from_secs(1),
            Arc::clone(&clock),
        );
        cache.insert(1, 5);
        assert_eq!(cache.get(&1), Some(5));
        clock.advance(Duration::from_secs(2));
        assert_eq!(cache.get(&1), None);
        assert_eq!(cache.entry_count().get(), 0);
    }

    #[test]
    fn the_governor_sheds_the_largest_registered_cache() {
        let weigh = |value: &Vec<u8>| Weight::new(value.len() as u64);
        let small: Arc<Lru<u32, Vec<u8>>> = Arc::new(Lru::by_weight(Weight::new(1_000_000), weigh));
        let big: Arc<Lru<u32, Vec<u8>>> = Arc::new(Lru::by_weight(Weight::new(1_000_000), weigh));
        small.insert(0, vec![0u8; 10]);
        big.insert(0, vec![0u8; 100]);
        let registry = Registry::default();
        registry.register(&small);
        registry.register(&big);
        let freed = registry
            .reclaim_largest()
            .expect("a registered cache is shed");
        assert_eq!(freed.get(), 100, "the largest footprint is reclaimed");
        assert_eq!(big.entry_count().get(), 0, "the largest cache is emptied");
        assert_eq!(
            small.entry_count().get(),
            1,
            "the smaller cache is untouched"
        );
    }

    #[test]
    fn a_registered_cache_with_nothing_to_reclaim_is_not_shed() {
        let registry = Registry::default();
        let cache: Arc<Lru<u32, Vec<u8>>> = Arc::new(Lru::by_count(EntryCount::new(8)));
        registry.register(&cache);
        assert_eq!(
            registry.reclaim_largest(),
            None,
            "an empty registered cache reports nothing to shed"
        );
    }

    #[test]
    fn the_noop_cache_never_retains() {
        let cache: &dyn Cache<u32, u32> = &Noop;
        cache.insert(1, 2);
        assert_eq!(cache.get(&1), None);
        assert_eq!(cache.entry_count().get(), 0);
    }
}
