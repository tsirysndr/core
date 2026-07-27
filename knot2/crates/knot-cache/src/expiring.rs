use std::collections::HashMap;
use std::collections::hash_map::Entry as Slot;
use std::hash::Hash;
use std::sync::Mutex;

use knot_runtime::UnixMicros;

knot_types::scalar_newtype! {
    pub struct GroupQuota(usize);
    pub struct TotalQuota(usize);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quotas {
    pub per_group: GroupQuota,
    pub total: TotalQuota,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    Total,
    Group,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admitted<V> {
    Inserted,
    Occupied(V),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Occupancy {
    Keep,
    Extend,
}

struct Entry<G, V> {
    group: G,
    value: V,
    expires_at: UnixMicros,
}

struct Inner<K, G, V> {
    entries: HashMap<K, Entry<G, V>>,
    group_counts: HashMap<G, usize>,
}

pub struct Expiring<K, G, V> {
    inner: Mutex<Inner<K, G, V>>,
    quotas: Quotas,
}

impl<K, G, V> Expiring<K, G, V>
where
    K: Eq + Hash + Clone,
    G: Eq + Hash + Clone,
{
    pub fn new(quotas: Quotas) -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                group_counts: HashMap::new(),
            }),
            quotas,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner<K, G, V>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn prune(&self, now: UnixMicros) -> Vec<K> {
        prune_locked(&mut self.lock(), now)
    }

    pub fn admit(
        &self,
        key: K,
        group: G,
        value: V,
        expires_at: UnixMicros,
        now: UnixMicros,
    ) -> Result<Admitted<V>, Rejected>
    where
        V: Clone + PartialEq,
    {
        self.enter(key, group, value, expires_at, now, Occupancy::Keep)
    }

    pub fn admit_or_renew(
        &self,
        key: K,
        group: G,
        value: V,
        expires_at: UnixMicros,
        now: UnixMicros,
    ) -> Result<Admitted<V>, Rejected>
    where
        V: Clone + PartialEq,
    {
        self.enter(key, group, value, expires_at, now, Occupancy::Extend)
    }

    fn enter(
        &self,
        key: K,
        group: G,
        value: V,
        expires_at: UnixMicros,
        now: UnixMicros,
        occupied: Occupancy,
    ) -> Result<Admitted<V>, Rejected>
    where
        V: Clone + PartialEq,
    {
        let mut inner = self.lock();
        let key = match inner.entries.entry(key) {
            Slot::Occupied(mut held) if held.get().expires_at > now => {
                let entry = held.get_mut();
                if occupied == Occupancy::Extend && entry.value == value {
                    entry.expires_at = expires_at;
                }
                return Ok(Admitted::Occupied(entry.value.clone()));
            }
            Slot::Occupied(held) => {
                let (key, entry) = held.remove_entry();
                release_group(&mut inner.group_counts, &entry.group);
                key
            }
            Slot::Vacant(free) => free.into_key(),
        };
        // This is lazy on purpose!
        // Pruning traverses every entry so we shouldn't do it on every
        // admission, better to do this O(1) quota check.
        if self.rejection(&inner, &group).is_some() {
            prune_locked(&mut inner, now);
        }
        match self.rejection(&inner, &group) {
            Some(rejected) => Err(rejected),
            None => {
                *inner.group_counts.entry(group.clone()).or_insert(0) += 1;
                inner.entries.insert(
                    key,
                    Entry {
                        group,
                        value,
                        expires_at,
                    },
                );
                Ok(Admitted::Inserted)
            }
        }
    }

    fn rejection(&self, inner: &Inner<K, G, V>, group: &G) -> Option<Rejected> {
        let held = inner.group_counts.get(group).copied().unwrap_or(0);
        match (
            held >= self.quotas.per_group.get(),
            inner.entries.len() >= self.quotas.total.get(),
        ) {
            (true, _) => Some(Rejected::Group),
            (_, true) => Some(Rejected::Total),
            (false, false) => None,
        }
    }

    pub fn remove(&self, key: &K) -> Option<V> {
        remove_locked(&mut self.lock(), key)
    }

    pub fn get(&self, key: &K, now: UnixMicros) -> Option<V>
    where
        V: Clone,
    {
        let inner = self.lock();
        inner
            .entries
            .get(key)
            .filter(|entry| entry.expires_at > now)
            .map(|entry| entry.value.clone())
    }

    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn group_len(&self, group: &G) -> usize {
        self.lock().group_counts.get(group).copied().unwrap_or(0)
    }
}

fn prune_locked<K, G, V>(inner: &mut Inner<K, G, V>, now: UnixMicros) -> Vec<K>
where
    K: Eq + Hash + Clone,
    G: Eq + Hash,
{
    let expired: Vec<K> = inner
        .entries
        .iter()
        .filter(|(_, entry)| entry.expires_at <= now)
        .map(|(key, _)| key.clone())
        .collect();
    expired.iter().for_each(|key| {
        remove_locked(inner, key);
    });
    expired
}

fn remove_locked<K, G, V>(inner: &mut Inner<K, G, V>, key: &K) -> Option<V>
where
    K: Eq + Hash,
    G: Eq + Hash,
{
    let entry = inner.entries.remove(key)?;
    release_group(&mut inner.group_counts, &entry.group);
    Some(entry.value)
}

fn release_group<G: Eq + Hash>(counts: &mut HashMap<G, usize>, group: &G) {
    if let Some(count) = counts.get_mut(group) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            counts.remove(group);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(micros: u64) -> UnixMicros {
        UnixMicros::new(micros)
    }

    fn store(per_group: usize, total: usize) -> Expiring<&'static str, &'static str, u8> {
        Expiring::new(Quotas {
            per_group: GroupQuota::new(per_group),
            total: TotalQuota::new(total),
        })
    }

    #[test]
    fn a_slot_stays_occupied_until_it_expires_and_is_then_free_to_take_over() {
        let store = store(8, 8);
        assert_eq!(
            store.admit("uni", "nel", 1, at(100), at(0)),
            Ok(Admitted::Inserted)
        );
        assert_eq!(
            store.admit("uni", "olaren", 2, at(900), at(50)),
            Ok(Admitted::Occupied(1)),
            "the caller decides whether an occupied slot is a replay or a renewal"
        );
        assert_eq!(store.get(&"uni", at(50)), Some(1));
        assert_eq!(
            store.get(&"uni", at(100)),
            None,
            "a replay guard must keep the original expiry or a replayed token renews its own window"
        );
        assert_eq!(
            store.admit("uni", "olaren", 2, at(300), at(100)),
            Ok(Admitted::Inserted),
            "expiry is inclusive so a slot expiring exactly now is available"
        );
        assert_eq!(
            store.group_len(&"nel"),
            0,
            "the takeover releases the old group's count"
        );
        assert_eq!(store.group_len(&"olaren"), 1);
    }

    #[test]
    fn admit_or_renew_extends_only_the_current_holder() {
        let store = store(8, 8);
        store
            .admit_or_renew("uni", "nel", 1, at(100), at(0))
            .unwrap();
        assert_eq!(
            store.admit_or_renew("uni", "olaren", 2, at(900), at(50)),
            Ok(Admitted::Occupied(1))
        );
        assert_eq!(
            store.get(&"uni", at(150)),
            None,
            "a caller that isn't the holder mustn't extend the lease"
        );
        store
            .admit_or_renew("uni", "nel", 1, at(300), at(200))
            .unwrap();
        assert_eq!(
            store.admit_or_renew("uni", "nel", 1, at(600), at(250)),
            Ok(Admitted::Occupied(1))
        );
        assert_eq!(
            store.get(&"uni", at(500)),
            Some(1),
            "renewing under the lock that read the entry leaves no window for a release \
             to drop the slot between the read and the renewal"
        );
    }

    #[test]
    fn each_quota_bounds_its_own_scope_and_names_itself_when_it_refuses() {
        let store = store(1, 1);
        store.admit("uni", "nel", 1, at(100), at(0)).unwrap();
        assert_eq!(
            store.admit("kelp", "nel", 2, at(100), at(0)),
            Err(Rejected::Group),
            "a rejection names the group limit first because the caller can act on its own quota"
        );
        assert_eq!(
            store.admit("kelp", "olaren", 2, at(100), at(0)),
            Err(Rejected::Total),
            "a distinct group has its own budget but still shares the total"
        );
        assert_eq!(
            store.admit("kelp", "olaren", 2, at(400), at(200)),
            Ok(Admitted::Inserted),
            "admit prunes the expired entry and takes the slot it freed"
        );
    }

    #[test]
    fn releasing_a_slot_by_expiry_or_by_hand_frees_its_group_budget() {
        let store = store(2, 8);
        store.admit("uni", "nel", 7, at(100), at(0)).unwrap();
        store.admit("kelp", "nel", 8, at(500), at(0)).unwrap();
        assert_eq!(store.prune(at(200)), vec!["uni"]);
        assert_eq!(store.len(), 1);
        assert_eq!(
            store.group_len(&"nel"),
            1,
            "pruning decrements the group count rather than rebuilding it"
        );
        assert_eq!(store.remove(&"kelp"), Some(8));
        assert_eq!(store.remove(&"kelp"), None);
        assert_eq!(
            store.group_len(&"nel"),
            0,
            "removing the last entry of a group frees its budget"
        );
        assert_eq!(
            store.admit("whelk", "nel", 9, at(600), at(200)),
            Ok(Admitted::Inserted)
        );
    }

    #[test]
    fn concurrent_admissions_at_the_quota_keep_group_counts_equal_to_what_is_stored() {
        let store = std::sync::Arc::new(Expiring::new(Quotas {
            per_group: GroupQuota::new(2),
            total: TotalQuota::new(3),
        }));
        let keys = ["uni", "kelp", "whelk", "clam", "conch", "limpet"];
        let groups = ["nel", "olaren"];
        let threads: Vec<_> = (0..8u64)
            .map(|thread| {
                let store = std::sync::Arc::clone(&store);
                std::thread::spawn(move || {
                    (0..2_000u64).for_each(|round| {
                        let key = keys[(thread + round) as usize % keys.len()];
                        let group = groups[(thread + round) as usize % groups.len()];
                        let now = at(round * 10);
                        let _ = store.admit(key, group, 1u8, at(round * 10 + 40), now);
                        store.prune(now);
                    });
                })
            })
            .collect();
        threads
            .into_iter()
            .for_each(|thread| thread.join().unwrap());
        let counted: usize = groups.iter().map(|group| store.group_len(group)).sum();
        assert_eq!(
            counted,
            store.len(),
            "a group count higher than what is stored locks its group out of every later admission"
        );
    }
}
