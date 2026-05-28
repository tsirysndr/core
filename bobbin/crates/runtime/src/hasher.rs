use std::hash::BuildHasher;

use ahash::{AHasher, RandomState};

use crate::{Entropy, OsEntropy};

#[derive(Clone, Debug)]
pub struct RuntimeHasher {
    inner: RandomState,
}

impl RuntimeHasher {
    pub fn from_entropy(entropy: &dyn Entropy) -> Self {
        Self {
            inner: RandomState::with_seeds(
                entropy.next_u64(),
                entropy.next_u64(),
                entropy.next_u64(),
                entropy.next_u64(),
            ),
        }
    }

    pub const fn from_seeds(k0: u64, k1: u64, k2: u64, k3: u64) -> Self {
        Self {
            inner: RandomState::with_seeds(k0, k1, k2, k3),
        }
    }
}

impl Default for RuntimeHasher {
    fn default() -> Self {
        Self::from_entropy(&OsEntropy)
    }
}

impl BuildHasher for RuntimeHasher {
    type Hasher = AHasher;

    fn build_hasher(&self) -> Self::Hasher {
        self.inner.build_hasher()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::OsEntropy;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct CountingEntropy(AtomicU64);

    impl Entropy for CountingEntropy {
        fn next_u64(&self) -> u64 {
            self.0.fetch_add(1, Ordering::SeqCst)
        }
    }

    #[test]
    fn same_seed_inputs_produce_identical_hashes() {
        let a = RuntimeHasher::from_seeds(1, 2, 3, 4);
        let b = RuntimeHasher::from_seeds(1, 2, 3, 4);
        assert_eq!(a.hash_one("limpet"), b.hash_one("limpet"));
        assert_eq!(a.hash_one(42_u64), b.hash_one(42_u64));
    }

    #[test]
    fn different_seeds_produce_different_hashes() {
        let a = RuntimeHasher::from_seeds(1, 2, 3, 4);
        let b = RuntimeHasher::from_seeds(5, 6, 7, 8);
        assert_ne!(
            a.hash_one("conch"),
            b.hash_one("conch"),
            "two seedings must not collapse to the same key state",
        );
    }

    #[test]
    fn from_entropy_consumes_four_words_in_call_order() {
        let entropy: Arc<dyn Entropy> = Arc::new(CountingEntropy(AtomicU64::new(100)));
        let hasher = RuntimeHasher::from_entropy(&*entropy);
        let direct = RuntimeHasher::from_seeds(100, 101, 102, 103);
        assert_eq!(
            hasher.hash_one("nautilus"),
            direct.hash_one("nautilus"),
            "from_entropy must seed by draining four u64s in order",
        );
    }

    #[test]
    fn os_entropy_seeds_a_usable_hashmap() {
        let hasher = RuntimeHasher::from_entropy(&OsEntropy);
        let mut map: HashMap<&str, u32, RuntimeHasher> = HashMap::with_hasher(hasher);
        map.insert("kelp", 1);
        map.insert("uni", 2);
        assert_eq!(map.get("kelp"), Some(&1));
        assert_eq!(map.get("uni"), Some(&2));
    }
}
