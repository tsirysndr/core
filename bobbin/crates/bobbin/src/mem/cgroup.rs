use bobbin_runtime::MemoryBudget;

const MEMORY_MAX_PATH: &str = "/sys/fs/cgroup/memory.max";
const MEMORY_HIGH_PATH: &str = "/sys/fs/cgroup/memory.high";
const MEMORY_CURRENT_PATH: &str = "/sys/fs/cgroup/memory.current";
const HIGH_WATERMARK_NUMERATOR: u64 = 8;
const HIGH_WATERMARK_DENOMINATOR: u64 = 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetSource {
    CgroupV2,
    Unconstrained,
    NotCgroupV2,
    MalformedLimit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParsedMax {
    Unlimited,
    Bytes(MemoryBudget),
    Malformed,
}

pub fn detect_budget() -> (Option<MemoryBudget>, BudgetSource) {
    match std::fs::read_to_string(MEMORY_MAX_PATH) {
        Ok(raw) => match parse_memory_max(&raw) {
            ParsedMax::Bytes(budget) => (Some(budget), BudgetSource::CgroupV2),
            ParsedMax::Unlimited => (None, BudgetSource::Unconstrained),
            ParsedMax::Malformed => (None, BudgetSource::MalformedLimit),
        },
        Err(_) => (None, BudgetSource::NotCgroupV2),
    }
}

fn parse_memory_max(raw: &str) -> ParsedMax {
    match raw.trim() {
        "max" => ParsedMax::Unlimited,
        bytes => bytes
            .parse::<u64>()
            .map(|n| ParsedMax::Bytes(MemoryBudget::new(n)))
            .unwrap_or(ParsedMax::Malformed),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HighWatermark(u64);

impl HighWatermark {
    pub fn from_budget(budget: MemoryBudget) -> Self {
        Self(budget.bytes() / HIGH_WATERMARK_DENOMINATOR * HIGH_WATERMARK_NUMERATOR)
    }

    pub const fn bytes(self) -> u64 {
        self.0
    }
}

pub fn try_set_high(budget: MemoryBudget) {
    let high = HighWatermark::from_budget(budget);
    match std::fs::write(MEMORY_HIGH_PATH, high.bytes().to_string()) {
        Ok(()) => tracing::info!(
            high_bytes = high.bytes(),
            "set memory.high throttle watermark"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            high_bytes = high.bytes(),
            "could not self-write memory.high, set it via compose or ops"
        ),
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub struct MemoryCurrent(u64);

impl MemoryCurrent {
    pub const fn bytes(self) -> u64 {
        self.0
    }
}

pub fn read_current() -> Option<MemoryCurrent> {
    std::fs::read_to_string(MEMORY_CURRENT_PATH)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(MemoryCurrent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_max_reads_as_no_budget() {
        assert_eq!(parse_memory_max("max\n"), ParsedMax::Unlimited);
    }

    #[test]
    fn numeric_limit_reads_as_budget() {
        assert_eq!(
            parse_memory_max("104857600\n"),
            ParsedMax::Bytes(MemoryBudget::new(104_857_600)),
        );
    }

    #[test]
    fn garbage_reads_as_malformed() {
        assert_eq!(parse_memory_max("not-a-number"), ParsedMax::Malformed);
    }

    #[test]
    fn high_watermark_is_eighty_percent_of_budget() {
        let high = HighWatermark::from_budget(MemoryBudget::new(100 * 1024 * 1024));
        assert_eq!(high.bytes(), 80 * 1024 * 1024);
    }
}
