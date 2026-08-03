mod admission;
mod cpu;
mod disk;
mod fsio;
mod mem;
mod slots;

pub use admission::{
    AdmitGuard, Burst, GlobalInflight, HostKey, HostPacer, LimitConfig, PeerPacer, PerPeerInflight,
    PreAuthLimiter, RateLimit, RefillMicros, Refusal, SubjectKey, SubjectPacer,
};
pub use cpu::{Saturate, ThreadCount, gix_thread_limit, map_chunks, map_spans, saturate, threads};
pub use disk::{
    BelowFloor, DiskFloorBytes, DiskGovernor, DiskReservation, FreeBytes, ReserveBytes,
    free_bytes as disk_free_bytes,
};
pub use fsio::{
    FileMode, FsError, atomic_write, atomic_write_bytes, clear_staging, clear_stale, clear_temps,
    fsync_path, staging_nonce,
};
pub use mem::{
    AvailableBytes, BudgetSource, ChurnBytes, ConnectivityObjects, DecayMs, MemoryBudget,
    MemoryHighBytes, PayloadBytes, WorkingSetBytes, advert_cache_bytes, available_bytes,
    cache_shed_warranted, decay_warrants_apply, externalize_connectivity, ingest_admits,
    ingest_admits_churn, ingest_base_budget, ingest_thread_limit, object_cache_bytes,
    pack_cache_bytes, target_decay,
};
pub use slots::{PackSlots, ReceiveSlots, ResolveSlots, SlotPermit, Slots};

#[derive(Clone, Copy, Debug, Default)]
pub struct Ceilings {
    pub max_threads: Option<ThreadCount>,
    pub max_memory: Option<MemoryBudget>,
}

#[derive(Clone, Copy, Debug)]
pub struct Report {
    pub threads: ThreadCount,
    pub memory: Option<MemoryBudget>,
    pub memory_source: BudgetSource,
    pub memory_high_bytes: Option<MemoryHighBytes>,
}

pub fn init(ceilings: Ceilings) -> Report {
    let threads = cpu::install(
        ceilings
            .max_threads
            .unwrap_or_else(|| ThreadCount::new(default_threads())),
    );
    let (memory, memory_source) = mem::install(ceilings.max_memory);
    let memory_high_bytes = mem::try_set_memory_high();
    Report {
        threads,
        memory,
        memory_source,
        memory_high_bytes,
    }
}

fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
}
