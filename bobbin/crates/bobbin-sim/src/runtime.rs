use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bobbin_edge_index::{CoverageWatch, EdgeStore};
use bobbin_ingest::{
    DEFAULT_INGEST_PARALLELISM, DisconnectSink, IngestConfig, IngestRuntime, RepoIdResolver,
    WarmingBuffer, WarmingShadowBuffer, run as run_ingest,
};
use bobbin_record_lru::NoopRecordStore;
use bobbin_runtime::{
    Clock, DEFAULT_MEM_WS_CAPACITY, MemHttpTransport, MemWsTransport, RuntimeHasher, SeededEntropy,
    SimClock, UnixMicros,
};
use bobbin_slingshot_client::SlingshotClient;
use bobbin_types::search::NoopSearchSink;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::report::{SimOutcome, SimReport};
use crate::workload::{Workload, WorkloadCtx};

#[derive(Clone, Debug)]
pub struct SimConfig {
    pub seed: u64,
    pub base_unix: UnixMicros,
    pub max_virtual_runtime: Duration,
    pub parallelism: NonZeroUsize,
    pub hydrant_base: Url,
    pub slingshot_base: Url,
    pub mem_ws_capacity: usize,
    pub warming_buffer_enabled: bool,
}

impl SimConfig {
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            base_unix: UnixMicros::new(1_700_000_000_000_000),
            max_virtual_runtime: Duration::from_secs(60),
            parallelism: DEFAULT_INGEST_PARALLELISM,
            hydrant_base: Url::parse("ws://hydrant.sim/").unwrap(),
            slingshot_base: Url::parse("http://slingshot.sim/").unwrap(),
            mem_ws_capacity: DEFAULT_MEM_WS_CAPACITY,
            warming_buffer_enabled: true,
        }
    }
}

pub struct Sim {
    config: SimConfig,
    workload: Box<dyn Workload>,
}

impl Sim {
    pub fn new(config: SimConfig, workload: Box<dyn Workload>) -> Self {
        Self { config, workload }
    }

    pub async fn run(self) -> SimReport {
        let SimConfig {
            seed,
            base_unix,
            max_virtual_runtime,
            parallelism,
            hydrant_base,
            slingshot_base,
            mem_ws_capacity,
            warming_buffer_enabled,
        } = self.config;

        let entropy = Arc::new(SeededEntropy::new(seed));
        let hasher = RuntimeHasher::from_entropy(&*entropy);
        let clock: Arc<dyn Clock> = Arc::new(SimClock::at(base_unix));

        let store = Arc::new(EdgeStore::new(hasher.clone()));
        let coverage = Arc::new(CoverageWatch::new());
        let records = Arc::new(NoopRecordStore);
        let cancel = CancellationToken::new();
        let consumer_too_slow_count = Arc::new(AtomicU64::new(0));
        let disconnects = Arc::new(DisconnectSink::new());
        let warming_shadow = Arc::new(WarmingShadowBuffer::new(hasher.clone()));
        let warming_buffer = Arc::new(WarmingBuffer::new(hasher.clone()));

        let workload_name = self.workload.name();
        let ctx = WorkloadCtx {
            seed,
            clock: clock.clone(),
            entropy: entropy.clone(),
            store: store.clone(),
            coverage: coverage.clone(),
            records: records.clone(),
            cancel: cancel.clone(),
            consumer_too_slow_count: consumer_too_slow_count.clone(),
        };
        let hooks = self.workload.build(ctx);

        let slingshot_http = MemHttpTransport::shared(hooks.slingshot.clone(), clock.clone());
        let slingshot_client = SlingshotClient::new(slingshot_base, slingshot_http)
            .expect("slingshot base url is valid");
        let resolver = Arc::new(RepoIdResolver::with_slingshot(
            slingshot_client,
            clock.clone(),
            hasher.clone(),
        ));

        let mem_ws = MemWsTransport::shared_with_capacity(hooks.hydrant.clone(), mem_ws_capacity);

        let ingest_runtime: IngestRuntime<NoopSearchSink> = IngestRuntime {
            store: store.clone(),
            issue_states: Arc::new(bobbin_edge_index::StateIndex::new(hasher.clone())),
            pull_statuses: Arc::new(bobbin_edge_index::StateIndex::new(hasher.clone())),
            coverage: coverage.clone(),
            search: Arc::new(NoopSearchSink),
            records: records.clone() as Arc<dyn bobbin_record_lru::RecordStore>,
            resolver: resolver.clone(),
            clock: clock.clone(),
            entropy: entropy.clone(),
            ws: mem_ws,
            cancel: cancel.clone(),
            disconnects: Some(disconnects.clone()),
            warming_shadow: Some(warming_shadow.clone()),
            warming_buffer: warming_buffer_enabled.then(|| warming_buffer.clone()),
            knot_registry: None,
            knot_gate: None,
        };
        let ingest_config = IngestConfig {
            hydrant_base,
            start_cursor: bobbin_edge_index::HydrantCursor::new(0),
            parallelism,
        };
        let mut ingest_handle = tokio::spawn(async move {
            let _ = run_ingest(ingest_config, ingest_runtime).await;
        });

        let script = hooks.script;
        let initial_report = tokio::select! {
            biased;
            _ = clock.sleep(max_virtual_runtime) => SimReport {
                workload: workload_name,
                seed,
                outcome: SimOutcome::TimedOut,
                virtual_runtime: max_virtual_runtime,
                virtual_clock_end: clock.now_unix_micros(),
                events_processed: coverage.snapshot().events_processed(),
                last_cursor: coverage.snapshot().last_cursor().raw(),
                edge_count: store.key_count() as u64,
                resolver_hits: 0,
                resolver_misses: 0,
                consumer_too_slow_count: consumer_too_slow_count.load(Ordering::Relaxed),
                disconnect_count: disconnects.count(),
                last_disconnect: disconnects.snapshot(),
                warming_shadow: warming_shadow.snapshot(),
                warming_buffer: warming_buffer.snapshot(),
                failure_reason: Some(format!(
                    "max_virtual_runtime {max_virtual_runtime:?} exhausted",
                )),
            },
            r = script => r,
        };

        cancel.cancel();
        let drain_deadline = clock.sleep(Duration::from_secs(30));
        tokio::pin!(drain_deadline);
        tokio::select! {
            _ = &mut drain_deadline => {
                ingest_handle.abort();
                let _ = ingest_handle.await;
            }
            res = &mut ingest_handle => {
                let _ = res;
            }
        }

        let stats = resolver.stats();
        SimReport {
            edge_count: store.key_count() as u64,
            events_processed: coverage.snapshot().events_processed(),
            last_cursor: coverage.snapshot().last_cursor().raw(),
            resolver_hits: stats.hits,
            resolver_misses: stats.miss_count(),
            consumer_too_slow_count: consumer_too_slow_count.load(Ordering::Relaxed),
            disconnect_count: disconnects.count(),
            last_disconnect: disconnects.snapshot(),
            warming_shadow: warming_shadow.snapshot(),
            warming_buffer: warming_buffer.snapshot(),
            ..initial_report
        }
    }
}
