use std::sync::atomic::{AtomicU64, Ordering};

// Golden gamma gem alert
const GOLDEN_GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

fn splitmix64(z: u64) -> u64 {
    let z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

pub trait Entropy: Send + Sync + 'static {
    fn next_u64(&self) -> u64;
    fn fill(&self, buffer: &mut [u8]);
    fn derive(&self, label: u64) -> Box<dyn Entropy>;
}

pub struct OsEntropy;

impl Entropy for OsEntropy {
    fn next_u64(&self) -> u64 {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).expect("OS entropy unavailable");
        u64::from_le_bytes(bytes)
    }

    fn fill(&self, buffer: &mut [u8]) {
        getrandom::fill(buffer).expect("OS entropy unavailable");
    }

    fn derive(&self, _label: u64) -> Box<dyn Entropy> {
        Box::new(OsEntropy)
    }
}

pub struct SeededEntropy {
    seed: u64,
    state: AtomicU64,
}

impl SeededEntropy {
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            state: AtomicU64::new(seed),
        }
    }

    pub fn derive(&self, label: u64) -> SeededEntropy {
        SeededEntropy::new(splitmix64(self.seed ^ splitmix64(label)))
    }
}

impl Entropy for SeededEntropy {
    fn next_u64(&self) -> u64 {
        let z = self
            .state
            .fetch_add(GOLDEN_GAMMA, Ordering::SeqCst)
            .wrapping_add(GOLDEN_GAMMA);
        splitmix64(z)
    }

    fn fill(&self, buffer: &mut [u8]) {
        buffer.chunks_mut(8).for_each(|chunk| {
            let value = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&value[..chunk.len()]);
        });
    }

    fn derive(&self, label: u64) -> Box<dyn Entropy> {
        Box::new(SeededEntropy::derive(self, label))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(entropy: &SeededEntropy, count: usize) -> Vec<u64> {
        std::iter::repeat_with(|| entropy.next_u64())
            .take(count)
            .collect()
    }

    #[test]
    fn seeded_entropy_is_deterministic() {
        let one = SeededEntropy::new(42);
        let two = SeededEntropy::new(42);
        assert_eq!(stream(&one, 256), stream(&two, 256));
    }

    #[test]
    fn distinct_seeds_diverge() {
        assert_ne!(
            stream(&SeededEntropy::new(1), 64),
            stream(&SeededEntropy::new(2), 64)
        );
    }

    #[test]
    fn derive_is_independent_of_parent_draw_timing() {
        let early = SeededEntropy::new(42);
        let child_before = early.derive(7);

        let late = SeededEntropy::new(42);
        let _ = stream(&late, 100);
        let child_after = late.derive(7);

        assert_eq!(stream(&child_before, 128), stream(&child_after, 128));
    }

    #[test]
    fn derived_streams_differ_by_label() {
        let parent = SeededEntropy::new(42);
        assert_ne!(stream(&parent.derive(1), 64), stream(&parent.derive(2), 64));
    }

    fn first_fill(entropy: &dyn Entropy, label: u64) -> [u8; 16] {
        let mut buffer = [0u8; 16];
        entropy.derive(label).fill(&mut buffer);
        buffer
    }

    #[test]
    fn trait_object_derive_is_independent_of_draw_order() {
        let parent: &dyn Entropy = &SeededEntropy::new(77);
        let in_order = [first_fill(parent, 10), first_fill(parent, 20)];

        let parent: &dyn Entropy = &SeededEntropy::new(77);
        let reversed = [first_fill(parent, 20), first_fill(parent, 10)];

        assert_eq!(in_order[0], reversed[1]);
        assert_eq!(in_order[1], reversed[0]);
        assert_ne!(in_order[0], in_order[1]);
    }

    #[test]
    fn seeded_fill_matches_stream() {
        let stream = SeededEntropy::new(7);
        let expected = stream.next_u64().to_le_bytes();
        let bytes = SeededEntropy::new(7);
        let mut buffer = [0u8; 8];
        bytes.fill(&mut buffer);
        assert_eq!(buffer, expected);
    }
}
