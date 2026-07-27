//! # Memory sensing & sizing.
//!
//! Everything is derived from:
//! - `MemoryBudget`: how much mem this process may use in total,
//!   which we detect once then cache.
//! - `AvailableBytes`: how much is free right now,
//!   which we read afresh every call.
//!
//! Sizing decisions that have to stay fixed for the lifetime of
//! a given process will refer to the mem budget,
//! while decisions that react to load will take a live reading
//! on the fly like a pulse.
//!
//! ```text
//!   MemoryBudget, which is detected then cached
//!
//!   configured  cgroup  MemTotal  the budget        source
//!   ----------  ------  --------  ----------------  --------------------
//!   set         any     any       configured        Configured
//!   -           reads   reads     the smaller       whichever won
//!   -           reads   -         the cgroup limit  CgroupV2 or CgroupV1
//!   -           -       reads     MemTotal          ProcMeminfo
//!   -           -       -         None              Unconstrained
//!
//!   AvailableBytes, read every call
//!
//!   cgroup v2 max -> anon, else v1 limit -> usage, else MemAvailable, else None
//!
//!   MemoryBudget ---> object_cache_bytes, pack_cache_bytes,
//!         \           ingest_base_budget
//!          +--------> target_decay
//!         /
//!   AvailableBytes -> ingest_thread_limit, ingest_admits,
//!                     ingest_admits_churn, externalize_connectivity
//! ```
//!
//! *Figure 1: where `MemoryBudget` & `AvailableBytes` originate, along with receivers.*
//!
//! In Figure 1,
//! a `-` means that source is absent or unreadable,
//! and `any` means we don't consult it at all.
//! The `cgroup` column is cgroup v2 `memory.max`,
//! or cgroup v1 `memory.limit_in_bytes` when the v2 file is
//! missing or just reads `max`.
//!
//! Every cgroup v2 reading reads this cgroup
//! up to root and takes the smallest `memory.max` it finds up the chain.
//! A `None` budget means unconstrained,
//! so every clamp returns its caller's
//! ceiling and `target_decay` reports a healthy interval.
//!
//! A `None` live-reading means the sensor is unavailable,
//! so every check takes the most permissive variant:
//! admission returns true, the thread limit stays at the CPU ceiling,
//! and `externalize_connectivity` returns false such that the
//! connectivity-map stays in RAM.
//!
//! Long story short, if the knot can't detect any limits it'll assume
//! it's allowed everything it can handle.
//!
//! In contrast,
//! `memory_high_target` uses neither of the above,
//! since `try_set_memory_high` passes it the cgroup v2 max directly,
//! which means a configured budget,
//! a `MemTotal` budget, and a cgroup v1 limit all don't affect it.
//!
//! `clamp_to_budget` will choose its cache size from a percentage of the budget,
//! between a floor and a ceiling.
//! Each of the following steps wins in some situation,
//! which Table 1 traces with the `object_cache_bytes` constants of
//! ceiling 64M, percent 2, floor 8M:
//!
//! ```text
//!   ceiling.min(max(budget / 100 * percent, floor)).min(budget)
//!
//!   budget  budget*pct  max(.,floor)  min(ceiling,.)  min(.,budget)  winner
//!   ------  ----------  ------------  --------------  -------------  -------
//!       4M       0.08M            8M              8M             4M  budget
//!     256M        5.1M            8M              8M             8M  floor
//!       1G       20.5M         20.5M           20.5M          20.5M  percent
//!       8G      163.8M        163.8M             64M            64M  ceiling
//! ```
//!
//! *Table 1: a budget per row, and which step decided it.*
//!
//! The 4M row is the only one where trailing `min` actually does anything,
//! since it covers a host whose entire budget is below the floor.
//!
//! `decay_for_headroom` maps headroom, meaning available over-budget,
//! onto the jemalloc dirty-page decay interval.
//!
//! // TODO: research if I can do this with mimalloc.
//!
//! When there's a lot of headroom, pages will stay cached for ten seconds,
//! but while under pressure the decay drops to zero such that pages go
//! back to the OS immediately.
//! Between those thresholds it interpolates like:
//!
//! ```text
//!   decay ms
//!    10000 |                  ------------------
//!          |               ,-'
//!          |            ,-'
//!          |         ,-'
//!          |      ,-'
//!          |   ,-'
//!        0 +===+-------------+-----------------+
//!          0% 10%           50%             100%
//!                  headroom = available / budget
//! ```
//!
//! *Figure 2: headroom mapped onto dirty-page decay interval.*
//!
//! That `=` run below 10% in Figure 2 is the curve itself,
//! I meant flat at zero, not the axis. :P
//! Between the thresholds the ramp climbs 250ms per point of headroom.
//!
//! `decay_warrants_apply` judges writes against the above curve.
//! Any move smaller than one second will be ignored,
//! so the reading has to shift like 4 points of
//! headroom before we rewrite the setting.
//! A target of 0 is exempt and always applies,
//! unless it happens to be the applied value already.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const CGROUP_V2_ROOT: &str = "/sys/fs/cgroup";
const PROC_SELF_CGROUP: &str = "/proc/self/cgroup";
const MEMORY_V1_LIMIT_PATH: &str = "/sys/fs/cgroup/memory/memory.limit_in_bytes";
const MEMORY_V1_USAGE_PATH: &str = "/sys/fs/cgroup/memory/memory.usage_in_bytes";
const MEMINFO_PATH: &str = "/proc/meminfo";

const CGROUP_V1_UNLIMITED: u64 = 0x7FFF_FFFF_FFFF_F000;

const HIGH_HEADROOM_PERCENT: u64 = 10;
const HIGH_HEADROOM_LIMIT: u64 = 1024 * 1024 * 1024;

const OBJECT_CACHE_CEILING: u64 = 64 * 1024 * 1024;
const OBJECT_CACHE_PERCENT: Percent = Percent::new(2);
const OBJECT_CACHE_FLOOR: u64 = 8 * 1024 * 1024;

const PACK_CACHE_PERCENT: Percent = Percent::new(25);
const PACK_CACHE_FLOOR: u64 = 32 * 1024 * 1024;

const ADVERT_CACHE_CEILING: u64 = 128 * 1024 * 1024;
const ADVERT_CACHE_PERCENT: Percent = Percent::new(5);
const ADVERT_CACHE_FLOOR: u64 = 8 * 1024 * 1024;

const CACHE_SHED_PERCENT: u64 = 10;

const INGEST_BASE_PERCENT: u64 = 40;

const DECAY_HEALTHY_MS: isize = 10_000;
const DECAY_PRESSURE_MS: isize = 0;
const DECAY_HYSTERESIS_MS: isize = 1_000;
const HEADROOM_RELAXED_PERCENT: u64 = 50;
const HEADROOM_TIGHT_PERCENT: u64 = 10;

const CONNECTIVITY_BYTES_PER_OBJECT: u64 = 96;
const INGEST_FIXED_BYTES: u64 = 16 * 1024 * 1024;
const INGEST_THREAD_WORKING_BYTES: u64 = 12 * 1024 * 1024;
const INGEST_CONCURRENCY_BYTES: u64 = 256 * 1024 * 1024;
const INGEST_CHURN_MULTIPLE: u64 = 4;

knot_types::scalar_newtype! {
    pub struct MemoryBudget(u64);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Percent(u64);

impl Percent {
    pub const fn new(percent: u64) -> Self {
        // Const so a `250` would fail build for example.
        // `clamp_to_budget` would otherwise have major problems
        // at runtime.
        assert!(percent <= 100, "percent exceeds 100");
        Self(percent)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetSource {
    Configured,
    CgroupV2,
    CgroupV1,
    ProcMeminfo,
    Unconstrained,
}

static BUDGET: OnceLock<(Option<MemoryBudget>, BudgetSource)> = OnceLock::new();

pub(crate) fn install(configured: Option<MemoryBudget>) -> (Option<MemoryBudget>, BudgetSource) {
    let computed = match configured {
        Some(budget) => (Some(budget), BudgetSource::Configured),
        None => detect_memory_budget(),
    };
    *BUDGET.get_or_init(|| computed)
}

fn resolved() -> Option<MemoryBudget> {
    BUDGET.get_or_init(detect_memory_budget).0
}

fn detect_cgroup_limit() -> Option<(u64, BudgetSource)> {
    read_cgroup_max()
        .map(|limit| (limit, BudgetSource::CgroupV2))
        .or_else(|| read_cgroup_v1_max().map(|limit| (limit, BudgetSource::CgroupV1)))
}

fn detect_memory_budget() -> (Option<MemoryBudget>, BudgetSource) {
    match (detect_cgroup_limit(), read_meminfo_total()) {
        (Some((cgroup, source)), Some(total)) => {
            if cgroup <= total {
                (Some(MemoryBudget::new(cgroup)), source)
            } else {
                (Some(MemoryBudget::new(total)), BudgetSource::ProcMeminfo)
            }
        }
        (Some((cgroup, source)), None) => (Some(MemoryBudget::new(cgroup)), source),
        (None, Some(total)) => (Some(MemoryBudget::new(total)), BudgetSource::ProcMeminfo),
        (None, None) => (None, BudgetSource::Unconstrained),
    }
}

fn cgroup_v2_dir() -> Option<PathBuf> {
    let content = std::fs::read_to_string(PROC_SELF_CGROUP).ok()?;
    let relative = content
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?
        .trim();
    Some(Path::new(CGROUP_V2_ROOT).join(relative.trim_start_matches('/')))
}

fn read_cgroup_max() -> Option<u64> {
    let root = Path::new(CGROUP_V2_ROOT);
    let mut dir = cgroup_v2_dir()?;
    let mut effective: Option<u64> = None;
    loop {
        if let Some(limit) = std::fs::read_to_string(dir.join("memory.max"))
            .ok()
            .and_then(|raw| parse_cgroup_max(&raw))
        {
            effective = Some(effective.map_or(limit, |current| current.min(limit)));
        }
        if dir == root {
            break;
        }
        match dir.parent() {
            Some(parent) if parent.starts_with(root) => dir = parent.to_path_buf(),
            _ => break,
        }
    }
    effective
}

fn parse_cgroup_max(raw: &str) -> Option<u64> {
    match raw.trim() {
        "max" => None,
        bytes => bytes.parse::<u64>().ok(),
    }
}

fn read_meminfo_total() -> Option<u64> {
    parse_meminfo_field(&std::fs::read_to_string(MEMINFO_PATH).ok()?, "MemTotal:")
}

fn parse_meminfo_field(raw: &str, key: &str) -> Option<u64> {
    raw.lines()
        .find_map(|line| line.strip_prefix(key))
        .and_then(|rest| rest.trim().strip_suffix("kB"))
        .and_then(|kb| kb.trim().parse::<u64>().ok())
        .map(|kb| kb.saturating_mul(1024))
}

fn read_u64_file(path: &str) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

fn read_memory_stat_field(stat: &str, key: &str) -> Option<u64> {
    stat.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some(name), Some(value)) if name == key => value.parse::<u64>().ok(),
            _ => None,
        }
    })
}

fn cgroup_v2_available() -> Option<u64> {
    let dir = cgroup_v2_dir()?;
    let stat = std::fs::read_to_string(dir.join("memory.stat")).ok()?;
    let anon = read_memory_stat_field(&stat, "anon")?;
    Some(read_cgroup_max()?.saturating_sub(anon))
}

fn read_cgroup_v1_max() -> Option<u64> {
    read_u64_file(MEMORY_V1_LIMIT_PATH).filter(|&limit| limit < CGROUP_V1_UNLIMITED)
}

fn cgroup_v1_available() -> Option<u64> {
    let limit = read_cgroup_v1_max()?;
    Some(limit.saturating_sub(read_u64_file(MEMORY_V1_USAGE_PATH)?))
}

fn meminfo_available() -> Option<u64> {
    parse_meminfo_field(
        &std::fs::read_to_string(MEMINFO_PATH).ok()?,
        "MemAvailable:",
    )
}

pub fn available_bytes() -> Option<AvailableBytes> {
    cgroup_v2_available()
        .or_else(cgroup_v1_available)
        .or_else(meminfo_available)
        .map(AvailableBytes::new)
}

fn clamp_to_budget(
    budget: Option<MemoryBudget>,
    ceiling: u64,
    percent: Percent,
    floor: u64,
) -> u64 {
    match budget {
        None => ceiling,
        Some(budget) => ceiling
            .min((budget.get() / 100 * percent.get()).max(floor))
            .min(budget.get()),
    }
}

pub fn object_cache_bytes() -> usize {
    let sized = clamp_to_budget(
        resolved(),
        OBJECT_CACHE_CEILING,
        OBJECT_CACHE_PERCENT,
        OBJECT_CACHE_FLOOR,
    );
    usize::try_from(sized).unwrap_or(usize::MAX)
}

pub fn pack_cache_bytes(configured: u64) -> u64 {
    clamp_to_budget(resolved(), configured, PACK_CACHE_PERCENT, PACK_CACHE_FLOOR)
}

pub fn advert_cache_bytes() -> u64 {
    clamp_to_budget(
        resolved(),
        ADVERT_CACHE_CEILING,
        ADVERT_CACHE_PERCENT,
        ADVERT_CACHE_FLOOR,
    )
}

pub fn cache_shed_warranted() -> bool {
    shed_warranted_at(available_bytes(), resolved())
}

fn shed_warranted_at(available: Option<AvailableBytes>, budget: Option<MemoryBudget>) -> bool {
    match (available, budget) {
        (Some(available), Some(budget)) if budget.get() > 0 => {
            available.get().saturating_mul(100) / budget.get() < CACHE_SHED_PERCENT
        }
        _ => false,
    }
}

fn ingest_base_budget_for(budget: MemoryBudget) -> usize {
    let sized = budget.get() / 100 * INGEST_BASE_PERCENT;
    usize::try_from(sized).unwrap_or(usize::MAX)
}

pub fn ingest_base_budget() -> Option<usize> {
    resolved().map(ingest_base_budget_for)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecayMs(isize);

impl DecayMs {
    pub const fn ms(self) -> isize {
        self.0
    }
}

fn decay_for_headroom(available: Option<AvailableBytes>, budget: Option<MemoryBudget>) -> DecayMs {
    let (available, budget) = match (available, budget) {
        (Some(available), Some(budget)) if budget.get() > 0 => (available.get(), budget.get()),
        _ => return DecayMs(DECAY_HEALTHY_MS),
    };
    let headroom_percent = available.saturating_mul(100) / budget;
    let decay = if headroom_percent >= HEADROOM_RELAXED_PERCENT {
        DECAY_HEALTHY_MS
    } else if headroom_percent <= HEADROOM_TIGHT_PERCENT {
        DECAY_PRESSURE_MS
    } else {
        let span = (HEADROOM_RELAXED_PERCENT - HEADROOM_TIGHT_PERCENT) as isize;
        let above = (headroom_percent - HEADROOM_TIGHT_PERCENT) as isize;
        DECAY_HEALTHY_MS * above / span
    };
    DecayMs(decay)
}

pub fn target_decay() -> DecayMs {
    decay_for_headroom(available_bytes(), resolved())
}

pub fn decay_warrants_apply(applied: DecayMs, target: DecayMs) -> bool {
    if applied == target {
        false
    } else if target.ms() == DECAY_PRESSURE_MS {
        true
    } else {
        (target.ms() - applied.ms()).abs() >= DECAY_HYSTERESIS_MS
    }
}

fn connectivity_fits(count: ConnectivityObjects, available: Option<AvailableBytes>) -> bool {
    match available {
        Some(available) => {
            count.get().saturating_mul(CONNECTIVITY_BYTES_PER_OBJECT) <= available.get() / 2
        }
        None => true,
    }
}

pub fn externalize_connectivity(count: ConnectivityObjects) -> bool {
    !connectivity_fits(count, available_bytes())
}

fn ingest_threads_for(ceiling: usize, available: Option<AvailableBytes>) -> usize {
    match available {
        Some(available) => {
            let funded =
                available.get().saturating_sub(INGEST_FIXED_BYTES) / INGEST_CONCURRENCY_BYTES;
            ceiling
                .min(usize::try_from(funded).unwrap_or(ceiling))
                .max(1)
        }
        None => ceiling,
    }
}

fn ingest_floor_for(ceiling: usize, available: AvailableBytes) -> u64 {
    INGEST_FIXED_BYTES
        + ingest_threads_for(ceiling, Some(available)) as u64 * INGEST_THREAD_WORKING_BYTES
}

fn ingest_admits_for(
    ceiling: usize,
    available: Option<AvailableBytes>,
    payload_bytes: PayloadBytes,
) -> bool {
    match available {
        Some(available) => {
            ingest_floor_for(ceiling, available).saturating_add(payload_bytes.get())
                <= available.get()
        }
        None => true,
    }
}

pub fn ingest_thread_limit() -> usize {
    ingest_threads_for(crate::cpu::ceiling(), available_bytes())
}

pub fn ingest_admits(payload: PayloadBytes) -> bool {
    ingest_admits_for(crate::cpu::ceiling(), available_bytes(), payload)
}

knot_types::scalar_newtype! {
    pub struct WorkingSetBytes(u64);
    pub struct ChurnBytes(u64);
    pub struct AvailableBytes(u64);
    pub struct PayloadBytes(u64);
    pub struct ConnectivityObjects(u64);
    pub struct MemoryHighBytes(u64);
}

fn ingest_admits_churn_for(
    ceiling: usize,
    available: Option<AvailableBytes>,
    working_set: WorkingSetBytes,
    churn: ChurnBytes,
) -> bool {
    match available {
        Some(available) => {
            ingest_admits_for(ceiling, Some(available), PayloadBytes::new(working_set.0))
                && churn.0 <= available.get().saturating_mul(INGEST_CHURN_MULTIPLE)
        }
        None => true,
    }
}

pub fn ingest_admits_churn(working_set: WorkingSetBytes, churn: ChurnBytes) -> bool {
    ingest_admits_churn_for(crate::cpu::ceiling(), available_bytes(), working_set, churn)
}

fn memory_high_target(max: u64) -> u64 {
    let headroom = (max / 100 * HIGH_HEADROOM_PERCENT).min(HIGH_HEADROOM_LIMIT);
    max.saturating_sub(headroom)
}

pub(crate) fn try_set_memory_high() -> Option<MemoryHighBytes> {
    let dir = cgroup_v2_dir()?;
    let high = memory_high_target(read_cgroup_max()?);
    std::fs::write(dir.join("memory.high"), high.to_string())
        .ok()
        .map(|()| MemoryHighBytes::new(high))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;

    #[test]
    fn an_unlimited_cgroup_reads_as_no_budget() {
        assert_eq!(parse_cgroup_max("max\n"), None);
        assert_eq!(parse_cgroup_max("104857600\n"), Some(104_857_600));
        assert_eq!(parse_cgroup_max("garbage"), None);
    }

    #[test]
    fn meminfo_fields_parse_kilobytes_into_bytes() {
        let sample = "MemTotal:   16384 kB\nMemFree: 100 kB\nMemAvailable:   8192 kB\n";
        assert_eq!(parse_meminfo_field(sample, "MemTotal:"), Some(16384 * 1024));
        assert_eq!(
            parse_meminfo_field(sample, "MemAvailable:"),
            Some(8192 * 1024)
        );
        assert_eq!(parse_meminfo_field(sample, "Nothing:"), None);
    }

    #[test]
    fn memory_stat_matches_the_whole_key_not_a_prefix() {
        let sample = "anon 2097152\nfile 8388608\nanon_thp 0\nkernel 65536\n";
        assert_eq!(read_memory_stat_field(sample, "anon"), Some(2_097_152));
        assert_eq!(read_memory_stat_field(sample, "file"), Some(8_388_608));
        assert_eq!(read_memory_stat_field(sample, "anon_thp"), Some(0));
        assert_eq!(read_memory_stat_field(sample, "missing"), None);
    }

    #[test]
    fn the_sensor_reads_live_memory_on_this_host() {
        let available = available_bytes()
            .expect("a Linux host must expose live memory availability")
            .get();
        assert!(
            available > 0,
            "available memory must be positive, got {available}"
        );
    }

    #[test]
    fn an_unconstrained_host_keeps_the_ceiling() {
        assert_eq!(
            clamp_to_budget(None, 500_000_000, Percent::new(25), 32),
            500_000_000
        );
    }

    #[test]
    fn a_constrained_host_clamps_to_the_fraction() {
        let budget = Some(MemoryBudget::new(400 * MIB));
        assert_eq!(
            clamp_to_budget(budget, 4 * GIB, PACK_CACHE_PERCENT, PACK_CACHE_FLOOR),
            100 * MIB
        );
    }

    #[test]
    fn a_tiny_host_holds_the_floor_but_never_exceeds_the_budget() {
        let budget = Some(MemoryBudget::new(16 * MIB));
        assert_eq!(
            clamp_to_budget(budget, 4 * GIB, PACK_CACHE_PERCENT, PACK_CACHE_FLOOR),
            16 * MIB
        );
    }

    #[test]
    fn a_small_host_reserves_the_headroom_percent() {
        let max = 4 * GIB;
        let headroom = max / 100 * HIGH_HEADROOM_PERCENT;
        assert!(headroom < HIGH_HEADROOM_LIMIT);
        assert_eq!(memory_high_target(max), max - headroom);
    }

    #[test]
    fn a_large_host_bounds_the_reclaim_headroom() {
        let max = 128 * GIB;
        assert!(max / 100 * HIGH_HEADROOM_PERCENT > HIGH_HEADROOM_LIMIT);
        assert_eq!(memory_high_target(max), max - HIGH_HEADROOM_LIMIT);
    }

    #[test]
    fn a_healthy_host_keeps_the_allocator_lazy_and_a_squeezed_one_reclaims() {
        let budget = Some(MemoryBudget::new(4 * GIB));
        assert_eq!(
            decay_for_headroom(Some(AvailableBytes::new(3 * GIB)), budget).ms(),
            DECAY_HEALTHY_MS,
            "ample headroom stays fast"
        );
        assert_eq!(
            decay_for_headroom(Some(AvailableBytes::new(GIB / 4)), budget).ms(),
            DECAY_PRESSURE_MS,
            "near-exhaustion reclaims at once"
        );
        assert_eq!(
            decay_for_headroom(Some(AvailableBytes::new(2 * GIB)), budget).ms(),
            DECAY_HEALTHY_MS,
            "half-free sits at the relaxed threshold"
        );
    }

    #[test]
    fn cache_shedding_triggers_only_under_tight_headroom() {
        let budget = Some(MemoryBudget::new(4 * GIB));
        assert!(
            !shed_warranted_at(Some(AvailableBytes::new(2 * GIB)), budget),
            "ample headroom keeps caches"
        );
        assert!(
            shed_warranted_at(Some(AvailableBytes::new(GIB / 4)), budget),
            "tight headroom sheds caches"
        );
        assert!(
            !shed_warranted_at(Some(AvailableBytes::new(GIB)), None),
            "an unmeasured budget never sheds"
        );
    }

    #[test]
    fn the_decay_interpolates_across_the_pressure_band() {
        let budget = Some(MemoryBudget::new(100 * MIB));
        assert_eq!(
            decay_for_headroom(Some(AvailableBytes::new(30 * MIB)), budget).ms(),
            DECAY_HEALTHY_MS * 20 / 40,
            "30% headroom is halfway through the 10..50 band"
        );
    }

    #[test]
    fn ingest_parallelism_backs_off_as_memory_tightens() {
        assert_eq!(
            ingest_threads_for(8, Some(AvailableBytes::new(4 * GIB))),
            8,
            "a roomy host keeps the full cpu ceiling"
        );
        assert_eq!(
            ingest_threads_for(8, Some(AvailableBytes::new(GIB))),
            3,
            "a 1GB limit funds only ~3 ingest threads, far below a many-core ceiling, so \
             decompression churn cannot outrun the munmap-on-free page return and grow unbounded"
        );
        assert_eq!(
            ingest_threads_for(8, Some(AvailableBytes::new(176 * MIB))),
            1,
            "a squeezed host drops to a single ingest thread, shrinking the working set"
        );
        assert_eq!(
            ingest_threads_for(8, None),
            8,
            "an unmeasurable host keeps the ceiling"
        );
    }

    #[test]
    fn the_base_spill_budget_stays_a_fraction_so_it_can_bound_a_small_host() {
        assert_eq!(
            ingest_base_budget_for(MemoryBudget::new(64 * GIB)),
            (64 * GIB / 100 * INGEST_BASE_PERCENT) as usize,
            "a roomy host spills only after the working set passes 40% of its RAM"
        );
        assert!(
            (ingest_base_budget_for(MemoryBudget::new(300 * MIB)) as u64) < 300 * MIB,
            "a squeezed host keeps the spill threshold under its total, or it OOMs before paging"
        );
    }

    #[test]
    fn ingest_admission_scales_its_floor_with_the_threads_it_will_actually_use() {
        assert!(
            ingest_admits_for(
                8,
                Some(AvailableBytes::new(32 * MIB)),
                PayloadBytes::new(MIB)
            ),
            "a small push fits a 32MB host by running a single ~28MB-floor ingest thread"
        );
        assert!(
            !ingest_admits_for(
                8,
                Some(AvailableBytes::new(20 * MIB)),
                PayloadBytes::new(MIB)
            ),
            "below the one-thread floor the push is shed, never OOM-ed part way through"
        );
        assert!(
            !ingest_admits_for(
                8,
                Some(AvailableBytes::new(64 * MIB)),
                PayloadBytes::new(200 * MIB)
            ),
            "a payload that dwarfs free memory is declined"
        );
        assert!(
            ingest_admits_for(8, None, PayloadBytes::new(u64::MAX)),
            "an unmeasurable host proceeds optimistically"
        );
    }

    #[test]
    fn ingest_churn_sheds_a_pack_whose_decompressed_volume_dwarfs_free_memory() {
        assert!(
            ingest_admits_churn_for(
                8,
                Some(AvailableBytes::new(GIB)),
                WorkingSetBytes(MIB),
                ChurnBytes(3 * GIB)
            ),
            "churn within a few multiples of free memory rides on the working-set floor"
        );
        assert!(
            !ingest_admits_churn_for(
                8,
                Some(AvailableBytes::new(GIB)),
                WorkingSetBytes(MIB),
                ChurnBytes(5 * GIB)
            ),
            "decompression volume past the multiple of free memory is shed, never OOM-ed"
        );
        assert!(
            !ingest_admits_churn_for(
                8,
                Some(AvailableBytes::new(20 * MIB)),
                WorkingSetBytes(MIB),
                ChurnBytes(MIB)
            ),
            "below the one-thread working floor the pack is shed even with trivial churn"
        );
        assert!(
            ingest_admits_churn_for(8, None, WorkingSetBytes(u64::MAX), ChurnBytes(u64::MAX)),
            "an unmeasurable host proceeds optimistically on both gates"
        );
    }

    #[test]
    fn connectivity_externalizes_only_when_the_in_ram_map_would_crowd_the_host() {
        assert!(
            connectivity_fits(
                ConnectivityObjects::new(1_000_000),
                Some(AvailableBytes::new(4 * GIB))
            ),
            "a small closure fits with headroom to spare"
        );
        assert!(
            !connectivity_fits(
                ConnectivityObjects::new(7_700_000),
                Some(AvailableBytes::new(512 * MIB))
            ),
            "nixpkgs cannot hold its connectivity map on a 512MB box"
        );
        assert!(
            connectivity_fits(ConnectivityObjects::new(u64::MAX), None),
            "an unmeasurable host stays on the fast in-ram path"
        );
    }

    #[test]
    fn an_unmeasurable_host_stays_on_the_fast_default() {
        assert_eq!(decay_for_headroom(None, None).ms(), DECAY_HEALTHY_MS);
        assert_eq!(
            decay_for_headroom(Some(AvailableBytes::new(GIB)), None).ms(),
            DECAY_HEALTHY_MS,
            "no budget means no pressure signal, so don't throttle"
        );
    }

    #[test]
    fn a_big_host_scales_up_to_the_ceiling_only() {
        let budget = Some(MemoryBudget::new(256 * GIB));
        assert_eq!(
            clamp_to_budget(
                budget,
                OBJECT_CACHE_CEILING,
                OBJECT_CACHE_PERCENT,
                OBJECT_CACHE_FLOOR
            ),
            OBJECT_CACHE_CEILING
        );
    }

    #[test]
    fn decay_hysteresis_absorbs_small_wobble_but_honors_the_pressure_floor() {
        let healthy = DecayMs(DECAY_HEALTHY_MS);
        assert!(
            !decay_warrants_apply(healthy, healthy),
            "an unchanged target never rewrites the arenas"
        );
        assert!(
            !decay_warrants_apply(DecayMs(5_000), DecayMs(5_200)),
            "a sub-band change is ignored so a percent of headroom wobble doesn't churn"
        );
        assert!(
            decay_warrants_apply(DecayMs(5_000), DecayMs(7_000)),
            "a change past the hysteresis band is applied"
        );
        assert!(
            decay_warrants_apply(DecayMs(200), DecayMs(DECAY_PRESSURE_MS)),
            "a move to the pressure floor is always honored so RSS reclaim is never delayed"
        );
    }
}
