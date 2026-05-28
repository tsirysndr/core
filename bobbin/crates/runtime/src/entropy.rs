use std::sync::atomic::{AtomicU64, Ordering};

pub trait Entropy: Send + Sync + 'static {
    fn next_u64(&self) -> u64;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct OsEntropy;

impl Entropy for OsEntropy {
    fn next_u64(&self) -> u64 {
        let mut buf = [0u8; 8];
        getrandom::fill(&mut buf).expect("os entropy source unavailable");
        u64::from_le_bytes(buf)
    }
}

#[derive(Debug)]
pub struct SeededEntropy {
    state: AtomicU64,
}

impl SeededEntropy {
    const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;

    pub fn new(seed: u64) -> Self {
        Self {
            state: AtomicU64::new(seed),
        }
    }
}

impl Entropy for SeededEntropy {
    fn next_u64(&self) -> u64 {
        let prev = self.state.fetch_add(Self::GOLDEN, Ordering::Relaxed);
        let z = prev.wrapping_add(Self::GOLDEN);
        let z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        let z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct CountingEntropy(AtomicU64);

    impl Entropy for CountingEntropy {
        fn next_u64(&self) -> u64 {
            self.0.fetch_add(1, Ordering::SeqCst)
        }
    }

    #[test]
    fn external_impl_works_behind_dyn_arc() {
        let e: Arc<dyn Entropy> = Arc::new(CountingEntropy(AtomicU64::new(100)));
        assert_eq!(e.next_u64(), 100);
        assert_eq!(e.next_u64(), 101);
    }

    #[test]
    fn seeded_entropy_two_constructions_with_same_seed_yield_same_stream() {
        let a = SeededEntropy::new(0xCAFE_F00D);
        let b = SeededEntropy::new(0xCAFE_F00D);
        for _ in 0..32 {
            assert_eq!(
                a.next_u64(),
                b.next_u64(),
                "splitmix stream must reproduce exactly across constructions with same seed",
            );
        }
    }

    #[test]
    fn seeded_entropy_different_seeds_diverge() {
        let a = SeededEntropy::new(1);
        let b = SeededEntropy::new(2);
        let mut all_match = true;
        for _ in 0..32 {
            if a.next_u64() != b.next_u64() {
                all_match = false;
                break;
            }
        }
        assert!(
            !all_match,
            "two seeded entropies with distinct seeds must diverge inside 32 draws",
        );
    }

    #[test]
    fn seeded_entropy_does_not_repeat_inside_short_window() {
        let e = SeededEntropy::new(0);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1024 {
            assert!(
                seen.insert(e.next_u64()),
                "splitmix collided inside 1024 draws"
            );
        }
    }

    #[test]
    fn os_entropy_does_not_collide_on_back_to_back_calls() {
        let e = OsEntropy;
        let a = e.next_u64();
        let b = e.next_u64();
        assert_ne!(
            a, b,
            "back-to-back os entropy collided, getrandom likely not actually wired up",
        );
    }
}
