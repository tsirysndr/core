use std::sync::Arc;
use std::time::Duration;

use bobbin_edge_index::{CoverageWatch, EdgeStore, StateIndex};
use bobbin_ingest::{IngestConfig, IngestRuntime, RepoIdResolver, run};
use bobbin_record_lru::{NoopRecordStore, RecordStore};
use bobbin_runtime::{OsEntropy, RuntimeHasher, SystemClock, TungsteniteWs};
use bobbin_types::search::NoopSearchSink;
use futures::stream::{self, StreamExt};
use tokio_util::sync::CancellationToken;
use url::Url;

const MIN_EVENTS_PER_RUN: u64 = 100;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    tracing_subscriber::fmt::try_init().ok();

    let endpoint = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ws://127.0.0.1:3010".to_owned());
    let ticks: u32 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(6);

    let url = Url::parse(&endpoint).expect("valid hydrant base url");
    let hasher = RuntimeHasher::from_entropy(&OsEntropy);
    let store = Arc::new(EdgeStore::new(hasher.clone()));
    let coverage = Arc::new(CoverageWatch::new());

    let cfg = IngestConfig::new(url);
    let cancel = CancellationToken::new();
    let runtime = IngestRuntime {
        store: store.clone(),
        issue_states: Arc::new(StateIndex::new(hasher.clone())),
        pull_statuses: Arc::new(StateIndex::new(hasher.clone())),
        coverage: coverage.clone(),
        search: Arc::new(NoopSearchSink),
        records: Arc::new(NoopRecordStore) as Arc<dyn RecordStore>,
        resolver: Arc::new(RepoIdResolver::detached(hasher)),
        clock: Arc::new(SystemClock::new()),
        entropy: Arc::new(OsEntropy),
        ws: TungsteniteWs::shared(),
        cancel: cancel.clone(),
        disconnects: None,
        warming_shadow: None,
        warming_buffer: None,
        knot_registry: None,
        knot_gate: None,
    };
    let task = tokio::spawn(async move {
        let _ = run(cfg, runtime).await;
    });

    stream::iter(0..ticks)
        .for_each(|i| {
            let store = store.clone();
            let coverage = coverage.clone();
            async move {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let snap = coverage.snapshot();
                println!(
                    "[{i:02}] coverage={snap:?} edge_keys={} sources={}",
                    store.key_count(),
                    store.source_count()
                );
            }
        })
        .await;

    cancel.cancel();
    let _ = task.await;

    let final_snap = coverage.snapshot();
    let events = final_snap.events_processed();
    println!(
        "done: events={events} edge_keys={} sources={} ready={}",
        store.key_count(),
        store.source_count(),
        final_snap.is_ready(),
    );
    assert!(
        events >= MIN_EVENTS_PER_RUN,
        "smoke received {events} events from {endpoint} across {ticks} five-second ticks, below threshold of {MIN_EVENTS_PER_RUN}; hydrant may be silent or unreachable",
    );
}
