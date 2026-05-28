use std::num::NonZeroUsize;

use bobbin_runtime::MemoryBudget;

const INGEST_BUDGET_PER_SLOT: u64 = 8 * 1024 * 1024;
const SEARCH_HEAP_BUDGET_PCT: u64 = 15;
const LRU_BUDGET_PCT: u64 = 20;
const MIN_SEARCH_HEAP_BYTES: u64 = 15 * 1024 * 1024;
const MIN_LRU_BYTES: u64 = 4 * 1024 * 1024;

pub fn ingest_parallelism(budget: Option<MemoryBudget>, configured: NonZeroUsize) -> NonZeroUsize {
    match budget {
        None => configured,
        Some(b) => {
            let slots = (b.bytes() / INGEST_BUDGET_PER_SLOT).max(1);
            let ceiling = usize::try_from(slots).unwrap_or(usize::MAX);
            NonZeroUsize::new(ceiling.min(configured.get())).unwrap_or(configured)
        }
    }
}

fn clamp_to_budget_fraction(
    budget: Option<MemoryBudget>,
    configured: u64,
    percent: u64,
    minimum: u64,
) -> u64 {
    match budget {
        None => configured,
        Some(b) => configured
            .min((b.bytes() / 100 * percent).max(minimum))
            .min(b.bytes()),
    }
}

pub fn search_heap_bytes(budget: Option<MemoryBudget>, configured: u64) -> u64 {
    clamp_to_budget_fraction(
        budget,
        configured,
        SEARCH_HEAP_BUDGET_PCT,
        MIN_SEARCH_HEAP_BYTES,
    )
}

pub fn lru_bytes(budget: Option<MemoryBudget>, configured: u64) -> u64 {
    clamp_to_budget_fraction(budget, configured, LRU_BUDGET_PCT, MIN_LRU_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    #[test]
    fn unconstrained_keeps_configured() {
        assert_eq!(ingest_parallelism(None, nz(16)), nz(16));
    }

    #[test]
    fn constrained_clamps_below_configured() {
        let budget = Some(MemoryBudget::new(50 * 1024 * 1024));
        assert_eq!(ingest_parallelism(budget, nz(16)), nz(6));
    }

    #[test]
    fn large_budget_never_raises_above_configured() {
        let budget = Some(MemoryBudget::new(4 * 1024 * 1024 * 1024));
        assert_eq!(ingest_parallelism(budget, nz(16)), nz(16));
    }

    #[test]
    fn caches_unclamped_when_unconstrained() {
        assert_eq!(search_heap_bytes(None, 50_000_000), 50_000_000);
        assert_eq!(lru_bytes(None, 67_108_864), 67_108_864);
    }

    #[test]
    fn caches_shrink_to_budget_fraction_when_constrained() {
        let budget = Some(MemoryBudget::new(100 * 1024 * 1024));
        assert_eq!(search_heap_bytes(budget, 50_000_000), 15 * 1024 * 1024);
        assert_eq!(lru_bytes(budget, 67_108_864), 20 * 1024 * 1024);
    }

    #[test]
    fn caches_honor_minimum_floor_on_tiny_budget() {
        let budget = Some(MemoryBudget::new(16 * 1024 * 1024));
        assert_eq!(search_heap_bytes(budget, 50_000_000), MIN_SEARCH_HEAP_BYTES);
        assert_eq!(lru_bytes(budget, 67_108_864), MIN_LRU_BYTES);
    }

    #[test]
    fn caches_never_raise_above_configured() {
        let budget = Some(MemoryBudget::new(4 * 1024 * 1024 * 1024));
        assert_eq!(search_heap_bytes(budget, 50_000_000), 50_000_000);
        assert_eq!(lru_bytes(budget, 67_108_864), 67_108_864);
    }
}
