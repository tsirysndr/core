use std::collections::{HashSet, VecDeque};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use bobbin_edge_index::{
    ApplyOutcome, Coverage, CoverageWatch, EdgeStore, HydrantCursor, IssueStateKind,
    PromotionSignal, PullStatusKind, StateIndex, apply_record_state,
};
use bobbin_knot_ingest::{CapabilityGate, KnotRegistry};
use bobbin_record_lru::RecordStore;
use bobbin_resolver::{NormalizeRepoRefs, decode_canon_or_upgrade_bytes, synthesize_created_at};
use bobbin_runtime::{
    Clock, Entropy, NetworkError, RuntimeHasher, UnixMicros, WsConn, WsMessage, WsStream,
    WsTransport,
};
use bobbin_types::edges::{Edge, ExtractError, Record};
use bobbin_types::ids::{RepoIdent, SubjectRef};
use bobbin_types::knot_acl::KnotHostKey;
use bobbin_types::record::RecordBody;
use bobbin_types::search::{SearchSink, SearchableRecord};
use bytes::Bytes;
use futures::StreamExt;
use jacquard_common::DefaultStr;
use jacquard_common::types::did::Did;
use jacquard_common::types::ident::AtIdentifier;
use jacquard_common::types::nsid::Nsid;
use jacquard_common::types::recordkey::Rkey;
use jacquard_common::types::string::{AtStrError, AtUri, Cid};
use thiserror::Error;
use tokio::time::Instant;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use url::Url;

mod frame;
mod resolver;
mod shadow;
mod warming;
use frame::HydrantStreamErrorFrame;
pub use frame::{FrameKind, HydrantFrame, RecordAction, RecordFrame};
pub use resolver::{RepoIdResolver, Resolution};
pub use shadow::{WarmingShadowBuffer, WarmingShadowSnapshot};
pub use warming::{ParkedUpsert, WarmingBuffer, WarmingBufferSnapshot};

const TANGLED_PREFIX: &str = "sh.tangled.";
const RECONNECT_INITIAL_DELAY: Duration = Duration::from_millis(500);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);
const PING_INTERVAL: Duration = Duration::from_secs(20);
const PONG_TIMEOUT: Duration = Duration::from_secs(15);
const READY_SKEW: Duration = Duration::from_secs(60);
const FRAME_CHANNEL_DEPTH: usize = 256;
const CONTROL_CHANNEL_DEPTH: usize = 16;
const READER_HOLD_LIMIT: usize = 64;
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const NORMAL_CLOSE: u16 = 1000;
const METRICS_DUMP_INTERVAL: Duration = Duration::from_secs(10);
const WARMING_FLUSH_PARALLELISM: usize = 64;
pub const DEFAULT_INGEST_PARALLELISM: NonZeroUsize = match NonZeroUsize::new(16) {
    Some(n) => n,
    None => unreachable!(),
};

#[derive(Clone, Debug)]
pub struct IngestConfig {
    pub hydrant_base: Url,
    pub start_cursor: HydrantCursor,
    pub parallelism: NonZeroUsize,
}

impl IngestConfig {
    pub fn new(hydrant_base: Url) -> Self {
        Self {
            hydrant_base,
            start_cursor: HydrantCursor::new(0),
            parallelism: DEFAULT_INGEST_PARALLELISM,
        }
    }

    fn stream_url(&self, cursor: HydrantCursor) -> Result<Url, IngestError> {
        let mut url = self.hydrant_base.clone();
        match url.scheme() {
            "http" => url
                .set_scheme("ws")
                .map_err(|_| IngestError::Url("set ws scheme"))?,
            "https" => url
                .set_scheme("wss")
                .map_err(|_| IngestError::Url("set wss scheme"))?,
            "ws" | "wss" => {}
            other => return Err(IngestError::UnknownScheme(other.to_owned())),
        }
        url.set_path("/stream");
        url.query_pairs_mut()
            .clear()
            .append_pair("cursor", &cursor.raw().to_string());
        Ok(url)
    }
}

#[derive(Debug, Error)]
pub enum IngestError {
    #[error("invalid hydrant url: {0}")]
    Url(&'static str),
    #[error("unsupported url scheme: {0}")]
    UnknownScheme(String),
    #[error("network: {0}")]
    Network(#[from] NetworkError),
    #[error("frame decode: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("invalid at-uri synthesized from frame: {0}")]
    InvalidAtUri(#[from] AtStrError),
    #[error("record extraction: {0}")]
    Extract(#[from] ExtractError),
    #[error("hydrant did not respond to ping within {0:?}")]
    PongTimeout(Duration),
    #[error("websocket send blocked for at least {0:?}, treating link as dead")]
    SendTimeout(Duration),
    #[error("hydrant disconnected because bobbin's stream consumer fell behind: {message}")]
    ConsumerTooSlow { message: String },
    #[error("hydrant signaled stream error {code}: {message}")]
    HydrantStream { code: String, message: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DisconnectKind {
    Url,
    UnknownScheme,
    Network,
    Decode,
    InvalidAtUri,
    Extract,
    PongTimeout,
    SendTimeout,
    ConsumerTooSlow,
    HydrantStream,
}

impl DisconnectKind {
    pub fn from_error(err: &IngestError) -> Self {
        match err {
            IngestError::Url(_) => Self::Url,
            IngestError::UnknownScheme(_) => Self::UnknownScheme,
            IngestError::Network(_) => Self::Network,
            IngestError::Decode(_) => Self::Decode,
            IngestError::InvalidAtUri(_) => Self::InvalidAtUri,
            IngestError::Extract(_) => Self::Extract,
            IngestError::PongTimeout(_) => Self::PongTimeout,
            IngestError::SendTimeout(_) => Self::SendTimeout,
            IngestError::ConsumerTooSlow { .. } => Self::ConsumerTooSlow,
            IngestError::HydrantStream { .. } => Self::HydrantStream,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisconnectSnapshot {
    pub kind: DisconnectKind,
    pub message: String,
    pub at_unix_micros: UnixMicros,
    pub last_cursor: HydrantCursor,
}

#[derive(Default)]
pub struct DisconnectSink {
    last: std::sync::Mutex<Option<DisconnectSnapshot>>,
    count: std::sync::atomic::AtomicU64,
}

impl DisconnectSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, snap: DisconnectSnapshot) {
        *self.last.lock().expect("disconnect sink mutex poisoned") = Some(snap);
        self.count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Option<DisconnectSnapshot> {
        self.last
            .lock()
            .expect("disconnect sink mutex poisoned")
            .clone()
    }

    pub fn count(&self) -> u64 {
        self.count.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionOutcome {
    Progressed,
    Empty,
}

#[derive(Debug)]
struct SessionEnd {
    outcome: SessionOutcome,
    error: Option<IngestError>,
}

pub struct IngestRuntime<S: SearchSink + 'static> {
    pub store: Arc<EdgeStore>,
    pub issue_states: Arc<StateIndex<IssueStateKind>>,
    pub pull_statuses: Arc<StateIndex<PullStatusKind>>,
    pub coverage: Arc<CoverageWatch>,
    pub search: Arc<S>,
    pub records: Arc<dyn RecordStore>,
    pub resolver: Arc<RepoIdResolver>,
    pub clock: Arc<dyn Clock>,
    pub entropy: Arc<dyn Entropy>,
    pub ws: Arc<dyn WsTransport>,
    pub cancel: CancellationToken,
    pub disconnects: Option<Arc<DisconnectSink>>,
    pub warming_shadow: Option<Arc<WarmingShadowBuffer>>,
    pub warming_buffer: Option<Arc<WarmingBuffer>>,
    pub knot_registry: Option<Arc<KnotRegistry>>,
    pub knot_gate: Option<Arc<CapabilityGate>>,
}

impl<S: SearchSink + 'static> Clone for IngestRuntime<S> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            issue_states: self.issue_states.clone(),
            pull_statuses: self.pull_statuses.clone(),
            coverage: self.coverage.clone(),
            search: self.search.clone(),
            records: self.records.clone(),
            resolver: self.resolver.clone(),
            clock: self.clock.clone(),
            entropy: self.entropy.clone(),
            ws: self.ws.clone(),
            cancel: self.cancel.clone(),
            disconnects: self.disconnects.clone(),
            warming_shadow: self.warming_shadow.clone(),
            warming_buffer: self.warming_buffer.clone(),
            knot_registry: self.knot_registry.clone(),
            knot_gate: self.knot_gate.clone(),
        }
    }
}

impl<S: SearchSink + 'static> IngestRuntime<S> {
    fn pipeline_ctx(&self) -> PipelineCtx<'_, S> {
        PipelineCtx {
            resolver: &self.resolver,
            store: &self.store,
            issue_states: &self.issue_states,
            pull_statuses: &self.pull_statuses,
            coverage: &self.coverage,
            records: &*self.records,
            search: &self.search,
            shadow: self.warming_shadow.as_deref(),
            buffer: self.warming_buffer.as_deref(),
            knot_registry: self.knot_registry.as_deref(),
            knot_gate: self.knot_gate.as_deref(),
        }
    }
}

struct PipelineCtx<'a, S: SearchSink + 'static> {
    resolver: &'a RepoIdResolver,
    store: &'a EdgeStore,
    issue_states: &'a StateIndex<IssueStateKind>,
    pull_statuses: &'a StateIndex<PullStatusKind>,
    coverage: &'a CoverageWatch,
    records: &'a dyn RecordStore,
    search: &'a S,
    shadow: Option<&'a WarmingShadowBuffer>,
    buffer: Option<&'a WarmingBuffer>,
    knot_registry: Option<&'a KnotRegistry>,
    knot_gate: Option<&'a CapabilityGate>,
}

pub async fn run<S: SearchSink + 'static>(
    config: IngestConfig,
    runtime: IngestRuntime<S>,
) -> Result<(), IngestError> {
    let metrics_dumper = spawn_metrics_dumper(&runtime);
    let idle_promoter = spawn_idle_promoter(&runtime);
    let warming_flusher = spawn_warming_flusher(&runtime);
    let result = run_inner(config, &runtime).await;
    if let Err(join) = metrics_dumper.await {
        warn!(?join, "metrics dumper task panicked");
    }
    if let Err(join) = idle_promoter.await {
        warn!(?join, "idle promoter task panicked");
    }
    if let Some(handle) = warming_flusher
        && let Err(join) = handle.await
    {
        warn!(?join, "warming flusher task panicked");
    }
    result
}

async fn run_inner<S: SearchSink + 'static>(
    config: IngestConfig,
    runtime: &IngestRuntime<S>,
) -> Result<(), IngestError> {
    let mut backoff = RECONNECT_INITIAL_DELAY;
    loop {
        let cursor = next_connect_cursor(runtime.coverage.snapshot(), config.start_cursor);
        let SessionEnd { outcome, error } = run_session(&config, cursor, runtime).await;
        if runtime.cancel.is_cancelled() {
            info!(
                last_cursor = runtime.coverage.snapshot().last_cursor().raw(),
                "ingest stopped after shutdown signal"
            );
            return Ok(());
        }
        match (outcome, &error) {
            (SessionOutcome::Progressed, None) => {
                info!("hydrant stream closed after delivering frames, reconnecting")
            }
            (SessionOutcome::Empty, None) => {
                warn!("hydrant stream closed without delivering frames")
            }
            (_, Some(err)) => warn!(?err, "hydrant stream errored"),
        }
        if let (Some(sink), Some(err)) = (runtime.disconnects.as_ref(), error.as_ref()) {
            sink.record(DisconnectSnapshot {
                kind: DisconnectKind::from_error(err),
                message: err.to_string(),
                at_unix_micros: runtime.clock.now_unix_micros(),
                last_cursor: runtime.coverage.snapshot().last_cursor(),
            });
        }
        let made_progress = matches!(outcome, SessionOutcome::Progressed);
        if made_progress {
            backoff = RECONNECT_INITIAL_DELAY;
        } else {
            tokio::select! {
                biased;
                _ = runtime.cancel.cancelled() => return Ok(()),
                _ = runtime.clock.sleep(jittered(backoff, &*runtime.entropy)) => {}
            }
            backoff = (backoff * 2).min(RECONNECT_MAX_DELAY);
        }
    }
}

fn spawn_warming_flusher<S: SearchSink + 'static>(
    runtime: &IngestRuntime<S>,
) -> Option<tokio::task::JoinHandle<()>> {
    runtime.warming_buffer.as_ref()?;
    let rt = runtime.clone();
    Some(tokio::spawn(async move {
        let buffer = rt
            .warming_buffer
            .as_deref()
            .expect("warming flusher only spawns when buffer is set");
        let mut rx = rt.coverage.subscribe();
        let reason = loop {
            if rx.borrow_and_update().is_ready() {
                break FlushReason::Ready;
            }
            tokio::select! {
                biased;
                _ = rt.cancel.cancelled() => break FlushReason::Cancelled,
                res = rx.changed() => match res {
                    Ok(()) => continue,
                    Err(_) => break FlushReason::CoverageDropped,
                },
            }
        };
        flush_warming_buffer(&rt, buffer, reason).await;
    }))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FlushReason {
    Ready,
    Cancelled,
    CoverageDropped,
}

impl FlushReason {
    fn as_str(self) -> &'static str {
        match self {
            FlushReason::Ready => "ready",
            FlushReason::Cancelled => "cancelled",
            FlushReason::CoverageDropped => "coverage_dropped",
        }
    }
}

async fn flush_warming_buffer<S: SearchSink + 'static>(
    runtime: &IngestRuntime<S>,
    buffer: &WarmingBuffer,
    reason: FlushReason,
) {
    let drained = buffer.drain_for_promote().await;
    if drained.is_empty() {
        return;
    }
    if reason != FlushReason::Ready {
        info!(
            target: "bobbin_ingest::warming",
            abandoned_entries = drained.len(),
            reason = reason.as_str(),
            "abandoning parked items on non-ready flush",
        );
        return;
    }
    let hasher = buffer.hasher().clone();
    let unique: HashSet<RepoIdent, RuntimeHasher> = drained
        .iter()
        .flat_map(|(_, deps)| deps.iter().cloned())
        .fold(HashSet::with_hasher(hasher), |mut acc, dep| {
            acc.insert(dep);
            acc
        });
    if !unique.is_empty() {
        let resolver = runtime.resolver.clone();
        let _: Vec<()> = futures::stream::iter(unique)
            .map(|key| {
                let resolver = resolver.clone();
                async move {
                    let _ = resolver.resolve(&key.owner, &key.rkey).await;
                }
            })
            .buffer_unordered(WARMING_FLUSH_PARALLELISM)
            .collect()
            .await;
    }
    let upserts: Vec<ParkedUpsert> = drained.into_iter().map(|(u, _)| u).collect();
    let count = upserts.len();
    let ctx = runtime.pipeline_ctx();
    finalize_drained(&ctx, upserts).await;
    info!(
        target: "bobbin_ingest::warming",
        flushed_entries = count,
        reason = reason.as_str(),
        "drained warming buffer",
    );
}

const IDLE_PROMOTE_WINDOW: Duration = Duration::from_secs(15);
const IDLE_PROMOTE_MIN_EVENTS: u64 = 256;

fn spawn_idle_promoter<S: SearchSink + 'static>(
    runtime: &IngestRuntime<S>,
) -> tokio::task::JoinHandle<()> {
    let rt = runtime.clone();
    tokio::spawn(async move {
        let mut prev = rt.coverage.snapshot().events_processed();
        loop {
            tokio::select! {
                biased;
                _ = rt.cancel.cancelled() => return,
                _ = rt.clock.sleep(IDLE_PROMOTE_WINDOW) => {}
            }
            let snap = rt.coverage.snapshot();
            if snap.is_ready() {
                return;
            }
            let processed = snap.events_processed();
            if processed >= IDLE_PROMOTE_MIN_EVENTS && processed == prev {
                rt.coverage.update(|c| c.force_ready());
                info!(
                    target: "bobbin_ingest::coverage",
                    events_processed = processed,
                    last_cursor = snap.last_cursor().raw(),
                    "stream idle, promoting coverage to ready",
                );
                return;
            }
            prev = processed;
        }
    })
}

fn spawn_metrics_dumper<S: SearchSink + 'static>(
    runtime: &IngestRuntime<S>,
) -> tokio::task::JoinHandle<()> {
    let rt = runtime.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = rt.cancel.cancelled() => break,
                _ = rt.clock.sleep(METRICS_DUMP_INTERVAL) => {
                    let s = rt.resolver.stats();
                    info!(
                        target: "bobbin_ingest::metrics",
                        resolver_hits = s.hits,
                        resolver_misses_mapped = s.misses_mapped,
                        resolver_misses_no_repo_did = s.misses_no_repo_did,
                        resolver_misses_unresolvable = s.misses_unresolvable,
                        resolver_misses_transient = s.misses_transient,
                        resolver_misses_no_client = s.misses_no_client,
                        resolver_miss_latency_micros_avg = s.miss_latency_micros_avg().unwrap_or(0),
                        resolver_miss_latency_micros_max = s.miss_latency_micros_max,
                        resolver_total = s.total(),
                        "resolver stats",
                    );
                }
            }
        }
    })
}

fn next_connect_cursor(snapshot: Coverage, start: HydrantCursor) -> HydrantCursor {
    if snapshot.events_processed() == 0 {
        start
    } else {
        HydrantCursor::new(snapshot.last_cursor().raw().saturating_add(1))
    }
}

fn jittered(base: Duration, entropy: &dyn Entropy) -> Duration {
    let base_ms = u64::try_from(base.as_millis()).unwrap_or(u64::MAX);
    let cap_ms = (base_ms / 4).max(1);
    base + Duration::from_millis(entropy.next_u64() % cap_ms)
}

async fn run_session<S: SearchSink + 'static>(
    config: &IngestConfig,
    cursor: HydrantCursor,
    runtime: &IngestRuntime<S>,
) -> SessionEnd {
    let url = match config.stream_url(cursor) {
        Ok(u) => u,
        Err(e) => {
            return SessionEnd {
                outcome: SessionOutcome::Empty,
                error: Some(e),
            };
        }
    };
    info!(%url, "connecting to hydrant /stream");
    let connect = tokio::select! {
        biased;
        _ = runtime.cancel.cancelled() => {
            return SessionEnd { outcome: SessionOutcome::Empty, error: None };
        }
        res = runtime.ws.connect(url) => res,
    };
    let WsConn {
        sink: mut ws_sink,
        stream: ws_stream,
    } = match connect {
        Ok(c) => c,
        Err(e) => {
            return SessionEnd {
                outcome: SessionOutcome::Empty,
                error: Some(IngestError::Network(e)),
            };
        }
    };

    let (frame_tx, frame_rx) = tokio::sync::mpsc::channel::<HydrantFrame>(FRAME_CHANNEL_DEPTH);
    let parallelism = config.parallelism.get();
    let processor_runtime = runtime.clone();
    let processor = tokio::spawn(async move {
        let cancel = processor_runtime.cancel.clone();
        let prep_rt = processor_runtime.clone();
        let resolve_rt = processor_runtime.clone();
        let commit_rt = processor_runtime;

        let pipeline = ReceiverStream::new(frame_rx)
            .map(move |frame| prep_stage(frame, prep_rt.clone()))
            .buffered(parallelism)
            .map(move |staged| resolve_stage(staged, resolve_rt.clone()))
            .buffered(parallelism)
            .for_each(move |staged| commit_stage(staged, commit_rt.clone(), parallelism));

        tokio::select! {
            biased;
            _ = cancel.cancelled() => {},
            _ = pipeline => {},
        }
    });

    let (control_tx, mut control_rx) = tokio::sync::mpsc::channel::<WsEvent>(CONTROL_CHANNEL_DEPTH);
    let session_cancel = runtime.cancel.child_token();
    let reader_cancel = session_cancel.clone();
    let reader = tokio::spawn(reader_loop(ws_stream, frame_tx, control_tx, reader_cancel));

    let mut next_ping = runtime.clock.now_instant() + PING_INTERVAL;
    let mut pong_deadline: Option<Instant> = None;

    let writer_error: Option<IngestError> = loop {
        tokio::select! {
            biased;
            _ = runtime.cancel.cancelled() => {
                let _ = timed_send(
                    &mut ws_sink,
                    WsMessage::Close { code: NORMAL_CLOSE, reason: "bobbin shutdown".to_owned() },
                ).await;
                break None;
            }
            _ = runtime.clock.sleep_until(next_ping) => {
                next_ping = runtime.clock.now_instant() + PING_INTERVAL;
                if pong_deadline.is_none() {
                    if let Err(e) = timed_send(&mut ws_sink, WsMessage::Ping(Bytes::new())).await {
                        break Some(e);
                    }
                    pong_deadline = Some(runtime.clock.now_instant() + PONG_TIMEOUT);
                }
            }
            _ = wait_until(pong_deadline, runtime.clock.as_ref()) => {
                break Some(IngestError::PongTimeout(PONG_TIMEOUT));
            }
            evt = control_rx.recv() => {
                let Some(evt) = evt else { break None; };
                match evt {
                    WsEvent::IncomingPing(payload) => {
                        if let Err(e) = timed_send(&mut ws_sink, WsMessage::Pong(payload)).await {
                            break Some(e);
                        }
                    }
                    WsEvent::IncomingPong => {
                        pong_deadline = None;
                    }
                }
            }
        }
    };

    session_cancel.cancel();
    drop(ws_sink);
    drop(control_rx);

    let reader_end = reader.await.unwrap_or(SessionEnd {
        outcome: SessionOutcome::Empty,
        error: None,
    });
    if let Err(join) = processor.await {
        warn!(?join, "frame processor task panicked");
    }
    SessionEnd {
        outcome: reader_end.outcome,
        error: writer_error.or(reader_end.error),
    }
}

async fn timed_send(
    sink: &mut Box<dyn bobbin_runtime::WsSink>,
    msg: WsMessage,
) -> Result<(), IngestError> {
    match tokio::time::timeout(SEND_TIMEOUT, sink.send(msg)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(IngestError::Network(e)),
        Err(_elapsed) => Err(IngestError::SendTimeout(SEND_TIMEOUT)),
    }
}

async fn reader_loop(
    mut ws_stream: Box<dyn WsStream>,
    frame_tx: tokio::sync::mpsc::Sender<HydrantFrame>,
    control_tx: tokio::sync::mpsc::Sender<WsEvent>,
    cancel: CancellationToken,
) -> SessionEnd {
    let mut outcome = SessionOutcome::Empty;
    let mut held: VecDeque<HydrantFrame> = VecDeque::new();

    let error: Option<IngestError> = loop {
        let step = if held.is_empty() {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => ReaderStep::Cancelled,
                msg = ws_stream.next() => ReaderStep::WsMessage(msg),
            }
        } else if held.len() < READER_HOLD_LIMIT {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => ReaderStep::Cancelled,
                permit_result = frame_tx.reserve() => match permit_result {
                    Ok(permit) => {
                        let frame = held
                            .pop_front()
                            .expect("non-empty held when reserve succeeds");
                        permit.send(frame);
                        ReaderStep::Sent
                    }
                    Err(_) => ReaderStep::FrameSinkClosed,
                },
                msg = ws_stream.next() => ReaderStep::WsMessage(msg),
            }
        } else {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => ReaderStep::Cancelled,
                permit_result = frame_tx.reserve() => match permit_result {
                    Ok(permit) => {
                        let frame = held
                            .pop_front()
                            .expect("held at limit when reserve succeeds");
                        permit.send(frame);
                        ReaderStep::Sent
                    }
                    Err(_) => ReaderStep::FrameSinkClosed,
                },
            }
        };

        match step {
            ReaderStep::Cancelled => break None,
            ReaderStep::FrameSinkClosed => break None,
            ReaderStep::Sent => {
                outcome = SessionOutcome::Progressed;
            }
            ReaderStep::WsMessage(msg) => {
                let Some(msg) = msg else {
                    break None;
                };
                let parsed = match msg {
                    Ok(m) => m,
                    Err(e) => break Some(IngestError::Network(e)),
                };
                match parsed {
                    WsMessage::Text(text) => {
                        let frame = match classify_text_frame(&text) {
                            Ok(f) => f,
                            Err(e) => break Some(e),
                        };
                        held.push_back(frame);
                    }
                    WsMessage::Binary(_) => {
                        debug!("hydrant sent unexpected binary frame, ignoring");
                    }
                    WsMessage::Ping(payload) => {
                        if control_tx
                            .send(WsEvent::IncomingPing(payload))
                            .await
                            .is_err()
                        {
                            break None;
                        }
                    }
                    WsMessage::Pong(_) => {
                        if control_tx.send(WsEvent::IncomingPong).await.is_err() {
                            break None;
                        }
                    }
                    WsMessage::Close { code, reason } => {
                        debug!(code, %reason, "hydrant closed stream");
                        break None;
                    }
                }
            }
        }
    };
    SessionEnd { outcome, error }
}

enum ReaderStep {
    Cancelled,
    FrameSinkClosed,
    Sent,
    WsMessage(Option<Result<WsMessage, NetworkError>>),
}

fn classify_text_frame(text: &str) -> Result<HydrantFrame, IngestError> {
    #[derive(serde::Deserialize)]
    struct PeekType<'a> {
        #[serde(rename = "type", borrow)]
        kind: Option<std::borrow::Cow<'a, str>>,
    }
    let is_error_frame = serde_json::from_str::<PeekType>(text)
        .ok()
        .and_then(|p| p.kind)
        .as_deref()
        == Some("error");
    if is_error_frame {
        return match serde_json::from_str::<HydrantStreamErrorFrame>(text) {
            Ok(err_frame) => Err(classify_hydrant_error(err_frame)),
            Err(decode_err) => Err(IngestError::Decode(decode_err)),
        };
    }
    serde_json::from_str::<HydrantFrame>(text).map_err(IngestError::Decode)
}

fn classify_hydrant_error(frame: HydrantStreamErrorFrame) -> IngestError {
    let HydrantStreamErrorFrame { error, message } = frame;
    let message = message.unwrap_or_default();
    match error.as_str() {
        "ConsumerTooSlow" => IngestError::ConsumerTooSlow { message },
        _ => IngestError::HydrantStream {
            code: error,
            message,
        },
    }
}

#[derive(Debug)]
enum WsEvent {
    IncomingPing(Bytes),
    IncomingPong,
}

async fn wait_until(deadline: Option<Instant>, clock: &dyn Clock) {
    match deadline {
        Some(d) => clock.sleep_until(d).await,
        None => std::future::pending::<()>().await,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Regime {
    Replay,
    Live,
    NonRecord,
}

impl Regime {
    fn as_str(self) -> &'static str {
        match self {
            Self::Replay => "replay",
            Self::Live => "live",
            Self::NonRecord => "non_record",
        }
    }
}

struct Pending {
    cursor: HydrantCursor,
    signal: PromotionSignal,
    regime: Regime,
    op: PendingOp,
}

enum PendingOp {
    Noop,
    ClearCache {
        source: AtUri<DefaultStr>,
    },
    Upsert(Box<UpsertPieces>),
    Parked {
        nsid: Nsid<DefaultStr>,
    },
    Delete {
        source: AtUri<DefaultStr>,
        nsid: Nsid<DefaultStr>,
    },
}

struct Prepared {
    pending: Pending,
    prepare_start: Instant,
    prepare_end: Instant,
}

struct Resolved {
    pending: Pending,
    prepare_start: Instant,
    prepare_end: Instant,
    resolve_start: Instant,
    resolve_end: Instant,
}

fn pending_nsid(op: &PendingOp) -> Option<&Nsid<DefaultStr>> {
    match op {
        PendingOp::Upsert(pieces) => Some(&pieces.nsid),
        PendingOp::Delete { nsid, .. } => Some(nsid),
        PendingOp::Parked { nsid, .. } => Some(nsid),
        PendingOp::Noop | PendingOp::ClearCache { .. } => None,
    }
}

fn pending_edge_count(op: &PendingOp) -> u64 {
    match op {
        PendingOp::Upsert(pieces) => pieces.edges.len() as u64,
        _ => 0,
    }
}

async fn prep_stage<S: SearchSink + 'static>(
    frame: HydrantFrame,
    rt: IngestRuntime<S>,
) -> Prepared {
    let now = rt.clock.now_unix_micros();
    let prepare_start = rt.clock.now_instant();
    let ctx = rt.pipeline_ctx();
    let pending = prepare_frame(frame, &ctx, now).await;
    let prepare_end = rt.clock.now_instant();
    Prepared {
        pending,
        prepare_start,
        prepare_end,
    }
}

async fn resolve_stage<S: SearchSink + 'static>(
    staged: Prepared,
    rt: IngestRuntime<S>,
) -> Resolved {
    let resolve_start = rt.clock.now_instant();
    let ctx = rt.pipeline_ctx();
    let pending = resolve_pending(staged.pending, &ctx).await;
    let resolve_end = rt.clock.now_instant();
    Resolved {
        pending,
        prepare_start: staged.prepare_start,
        prepare_end: staged.prepare_end,
        resolve_start,
        resolve_end,
    }
}

async fn commit_stage<S: SearchSink + 'static>(
    staged: Resolved,
    rt: IngestRuntime<S>,
    parallelism: usize,
) {
    let nsid = pending_nsid(&staged.pending.op).cloned();
    let edge_count = pending_edge_count(&staged.pending.op);
    let regime = staged.pending.regime;
    let cursor = staged.pending.cursor.raw();
    let Resolved {
        pending,
        prepare_start,
        prepare_end,
        resolve_start,
        resolve_end,
    } = staged;
    let commit_start = rt.clock.now_instant();
    commit_pending(
        pending,
        &rt.store,
        &rt.issue_states,
        &rt.pull_statuses,
        &rt.coverage,
        &*rt.search,
        &*rt.records,
        &rt.resolver,
    )
    .await;
    let commit_end = rt.clock.now_instant();
    tracing::trace!(
        target: "bobbin_ingest::stage",
        cursor,
        regime = regime.as_str(),
        nsid = %nsid.as_ref().map(Nsid::as_str).unwrap_or(""),
        edge_count,
        prepare_us = prepare_end.duration_since(prepare_start).as_micros() as u64,
        queue_resolve_wait_us = resolve_start.duration_since(prepare_end).as_micros() as u64,
        resolve_us = resolve_end.duration_since(resolve_start).as_micros() as u64,
        queue_commit_wait_us = commit_start.duration_since(resolve_end).as_micros() as u64,
        commit_us = commit_end.duration_since(commit_start).as_micros() as u64,
        total_us = commit_end.duration_since(prepare_start).as_micros() as u64,
        parallelism,
        "pipeline stage timings",
    );
}

async fn prepare_frame<S: SearchSink + 'static>(
    frame: HydrantFrame,
    ctx: &PipelineCtx<'_, S>,
    now: UnixMicros,
) -> Pending {
    let cursor = HydrantCursor::new(frame.id);
    let signal = promotion_signal(frame.record.as_ref(), now);
    let regime = match frame.record.as_ref() {
        Some(r) if r.live => Regime::Live,
        Some(_) => Regime::Replay,
        None => Regime::NonRecord,
    };
    let op = match frame.kind {
        FrameKind::Record => prepare_record(frame.record, ctx).await,
        FrameKind::Identity | FrameKind::Account => PendingOp::Noop,
        FrameKind::Other => {
            debug!(id = frame.id, "ignoring unknown hydrant frame kind");
            PendingOp::Noop
        }
    };
    Pending {
        cursor,
        signal,
        regime,
        op,
    }
}

async fn prepare_record<S: SearchSink + 'static>(
    record: Option<RecordFrame>,
    ctx: &PipelineCtx<'_, S>,
) -> PendingOp {
    let Some(record) = record else {
        debug!("record-typed frame missing payload, skipping");
        return PendingOp::Noop;
    };
    if !record.collection.as_ref().starts_with(TANGLED_PREFIX) {
        return PendingOp::Noop;
    }
    let nsid = record.collection.clone();
    let source = match build_source_uri(&record) {
        Ok(s) => s,
        Err(e) => {
            warn!(?e, "invalid frame source, dropping record");
            return PendingOp::Noop;
        }
    };
    match record.action {
        RecordAction::Create | RecordAction::Update => {
            evict_from_buffer(ctx.buffer, &source).await;
            let Some(raw) = record.record else {
                debug!(collection = %nsid, "create/update missing record body, clearing cache");
                return PendingOp::ClearCache { source };
            };
            let raw_bytes = Bytes::copy_from_slice(raw.get().as_bytes());
            let wire_bytes = match fallback_rfc3339(&record.rkey, &record.rev)
                .and_then(|fallback| synthesize_created_at(&raw_bytes, &fallback))
            {
                Some(patched) => Bytes::from(patched),
                None => raw_bytes,
            };
            let (parsed, bytes) = match decode_canon_or_upgrade_bytes(
                &record.collection,
                &wire_bytes,
                ctx.resolver,
            )
            .await
            {
                Ok((parsed, canon_bytes)) => {
                    let bytes = match canon_bytes {
                        std::borrow::Cow::Borrowed(_) => wire_bytes,
                        std::borrow::Cow::Owned(v) => Bytes::from(v),
                    };
                    (parsed, bytes)
                }
                Err(ExtractError::UnknownCollection(name)) => {
                    debug!(collection = %name, "unknown sh.tangled.* collection, clearing cache");
                    return PendingOp::ClearCache { source };
                }
                Err(e) => {
                    warn!(?e, collection = %record.collection, "record decode failed, clearing cache");
                    return PendingOp::ClearCache { source };
                }
            };
            if let Record::Repo(repo) = &parsed {
                if let Some(shadow) = ctx.shadow {
                    shadow.note_observed(&record.did, &record.rkey).await;
                }
                let superseded = ctx
                    .resolver
                    .observe(
                        record.did.clone(),
                        record.rkey.clone(),
                        repo.repo_did.clone(),
                    )
                    .await;
                if let Some(prior) = superseded {
                    let prior_uri = format!(
                        "at://{}/sh.tangled.repo/{}",
                        prior.owner.as_ref(),
                        prior.rkey.as_ref(),
                    );
                    if let Ok(prior_at_uri) = AtUri::<DefaultStr>::new_owned(&prior_uri) {
                        evict_from_buffer(ctx.buffer, &prior_at_uri).await;
                        ctx.store.remove_source(&prior_at_uri);
                        ctx.records.remove(&prior_at_uri);
                        ctx.search.remove(&prior_at_uri).await;
                    }
                }
                if let Some(buffer) = ctx.buffer {
                    let drained = buffer.take_observed(&record.did, &record.rkey).await;
                    if !drained.is_empty() {
                        finalize_drained(ctx, drained).await;
                    }
                }
                if let Some(registry) = ctx.knot_registry {
                    let host = KnotHostKey::new(repo.knot.as_ref());
                    match repo.repo_did.clone() {
                        Some(repo_did) => registry.observe_repo(&host, repo_did),
                        None => registry.observe_host(&host),
                    }
                }
            }
            match acl_disposition(&parsed, ctx.knot_gate, ctx.knot_registry) {
                AclDisposition::NativeSkip => {
                    if let Some(registry) = ctx.knot_registry {
                        registry.forget_legacy_member(&source);
                    }
                    return PendingOp::Delete { source, nsid };
                }
                AclDisposition::LegacyMember { host } => {
                    if let Some(registry) = ctx.knot_registry {
                        registry.observe_host(&host);
                        registry.note_legacy_member(source.clone(), &host);
                    }
                }
                AclDisposition::Other => {}
            }
            let edges = match parsed.extract_edges(&source) {
                Ok(es) => es,
                Err(e) => {
                    warn!(?e, "edge extraction failed, clearing cache");
                    return PendingOp::ClearCache { source };
                }
            };
            let _ = ctx.store.intern_source(&source);
            PendingOp::Upsert(Box::new(UpsertPieces {
                source,
                nsid,
                parsed,
                bytes,
                cid: record.cid,
                edges,
            }))
        }
        RecordAction::Delete => {
            evict_from_buffer(ctx.buffer, &source).await;
            if nsid.as_ref() == "sh.tangled.repo" {
                ctx.resolver.forget(&record.did, &record.rkey).await;
            }
            if nsid.as_ref() == "sh.tangled.knot.member"
                && let Some(registry) = ctx.knot_registry
            {
                registry.forget_legacy_member(&source);
            }
            PendingOp::Delete { source, nsid }
        }
        RecordAction::Other => {
            debug!(collection = %nsid, "ignoring unknown record action");
            PendingOp::Noop
        }
    }
}

enum AclDisposition {
    Other,
    NativeSkip,
    LegacyMember { host: KnotHostKey },
}

fn acl_disposition(
    parsed: &Record,
    gate: Option<&CapabilityGate>,
    registry: Option<&KnotRegistry>,
) -> AclDisposition {
    let Some(gate) = gate else {
        return AclDisposition::Other;
    };
    match parsed {
        Record::KnotMember(member) => {
            let host = KnotHostKey::new(member.domain.as_ref());
            if gate.is_native(&host) {
                AclDisposition::NativeSkip
            } else {
                AclDisposition::LegacyMember { host }
            }
        }
        Record::Collaborator(collaborator) => {
            let native = registry
                .and_then(|registry| registry.host_of_repo(&collaborator.repo))
                .is_some_and(|host| gate.is_native(&host));
            if native {
                AclDisposition::NativeSkip
            } else {
                AclDisposition::Other
            }
        }
        _ => AclDisposition::Other,
    }
}

fn fallback_rfc3339(
    rkey: &Rkey<DefaultStr>,
    rev: &jacquard_common::types::tid::Tid,
) -> Option<String> {
    let tid = jacquard_common::types::tid::Tid::new(rkey.as_ref())
        .ok()
        .unwrap_or_else(|| rev.clone());
    let micros = i64::try_from(tid.timestamp()).ok()?;
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(micros)?;
    Some(dt.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
}

async fn evict_from_buffer(buffer: Option<&WarmingBuffer>, source: &AtUri<DefaultStr>) {
    if let Some(buffer) = buffer
        && !buffer.is_sealed()
    {
        buffer.evict_source(source).await;
    }
}

async fn resolve_pending<S: SearchSink + 'static>(
    pending: Pending,
    ctx: &PipelineCtx<'_, S>,
) -> Pending {
    let Pending {
        cursor,
        signal,
        regime,
        op,
    } = pending;
    let op = match op {
        PendingOp::Upsert(pieces) => match try_park_warming(ctx, cursor, pieces).await {
            ParkOutcome::Parked { nsid } => PendingOp::Parked { nsid },
            ParkOutcome::Passthrough(mut pieces) => {
                let edges = std::mem::take(&mut pieces.edges);
                pieces.edges =
                    normalize_subjects(edges, ctx.resolver, ctx.coverage, ctx.shadow).await;
                PendingOp::Upsert(pieces)
            }
        },
        other => other,
    };
    Pending {
        cursor,
        signal,
        regime,
        op,
    }
}

struct UpsertPieces {
    source: AtUri<DefaultStr>,
    nsid: Nsid<DefaultStr>,
    parsed: Record,
    bytes: Bytes,
    cid: Option<Cid<DefaultStr>>,
    edges: Vec<Edge>,
}

impl From<ParkedUpsert> for UpsertPieces {
    fn from(u: ParkedUpsert) -> Self {
        Self {
            source: u.source,
            nsid: u.nsid,
            parsed: u.parsed,
            bytes: u.bytes,
            cid: u.cid,
            edges: u.edges,
        }
    }
}

enum ParkOutcome {
    Parked { nsid: Nsid<DefaultStr> },
    Passthrough(Box<UpsertPieces>),
}

async fn try_park_warming<S: SearchSink + 'static>(
    ctx: &PipelineCtx<'_, S>,
    cursor: HydrantCursor,
    pieces: Box<UpsertPieces>,
) -> ParkOutcome {
    let Some(buffer) = ctx.buffer else {
        return ParkOutcome::Passthrough(pieces);
    };
    if ctx.coverage.snapshot().is_ready() || buffer.is_sealed() {
        return ParkOutcome::Passthrough(pieces);
    }
    let deps = collect_unresolved_deps(&pieces.edges, ctx.resolver).await;
    if deps.is_empty() {
        return ParkOutcome::Passthrough(pieces);
    }
    let nsid = pieces.nsid.clone();
    let pieces = *pieces;
    let upsert = ParkedUpsert {
        cursor,
        source: pieces.source,
        nsid: pieces.nsid,
        parsed: pieces.parsed,
        bytes: pieces.bytes,
        cid: pieces.cid,
        edges: pieces.edges,
    };
    let deps_for_shadow = ctx.shadow.is_some().then(|| deps.clone());
    match buffer.try_park(upsert, deps).await {
        Ok(()) => {
            if let Some((shadow, noted)) = ctx.shadow.zip(deps_for_shadow) {
                let _ = futures::future::join_all(noted.into_iter().map(|dep| async move {
                    shadow.note_unresolved(dep.owner, dep.rkey).await;
                }))
                .await;
            }
            ParkOutcome::Parked { nsid }
        }
        Err(returned) => ParkOutcome::Passthrough(Box::new(returned.into())),
    }
}

async fn collect_unresolved_deps(edges: &[Edge], resolver: &RepoIdResolver) -> Vec<RepoIdent> {
    futures::stream::iter(edges)
        .fold(Vec::new(), |mut acc, edge| async move {
            let Some(uri) = edge.subject.as_uri() else {
                return acc;
            };
            let Some((owner, rkey)) = parse_repo_subject_uri(uri) else {
                return acc;
            };
            if resolver.cached_resolution(&owner, &rkey).await.is_some() {
                return acc;
            }
            let candidate = RepoIdent::new(owner, rkey);
            if !acc.contains(&candidate) {
                acc.push(candidate);
            }
            acc
        })
        .await
}

async fn finalize_drained<S: SearchSink + 'static>(
    ctx: &PipelineCtx<'_, S>,
    drained: Vec<ParkedUpsert>,
) {
    for upsert in drained {
        let ParkedUpsert {
            cursor: _,
            source,
            nsid: _,
            parsed,
            bytes,
            cid,
            edges,
        } = upsert;
        let edges = normalize_subjects(edges, ctx.resolver, ctx.coverage, None).await;
        cache_body(ctx.records, &source, cid, bytes);
        ctx.store.upsert_source(&source, edges);
        let outcome = apply_record_state(ctx.issue_states, ctx.pull_statuses, &source, &parsed);
        log_unknown_state_variant(outcome, &source);
        index_search(ctx.search, ctx.resolver, &source, parsed).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn commit_pending<S: SearchSink>(
    pending: Pending,
    store: &EdgeStore,
    issue_states: &StateIndex<IssueStateKind>,
    pull_statuses: &StateIndex<PullStatusKind>,
    coverage: &CoverageWatch,
    search: &S,
    records: &dyn RecordStore,
    resolver: &RepoIdResolver,
) {
    let Pending {
        cursor,
        signal,
        regime: _,
        op,
    } = pending;
    match op {
        PendingOp::Noop | PendingOp::Parked { .. } => {}
        PendingOp::ClearCache { source } => records.remove(&source),
        PendingOp::Upsert(pieces) => {
            let UpsertPieces {
                source,
                nsid: _,
                parsed,
                bytes,
                cid,
                edges,
            } = *pieces;
            cache_body(records, &source, cid, bytes);
            store.upsert_source(&source, edges);
            let outcome = apply_record_state(issue_states, pull_statuses, &source, &parsed);
            log_unknown_state_variant(outcome, &source);
            index_search(search, resolver, &source, parsed).await;
        }
        PendingOp::Delete { source, nsid } => {
            store.remove_source(&source);
            apply_delete_to_state_index(issue_states, pull_statuses, &source, &nsid);
            records.remove(&source);
            search.remove(&source).await;
        }
    }
    coverage.update(|c| c.advance(cursor).maybe_promote(signal));
}

async fn index_search<S: SearchSink>(
    search: &S,
    resolver: &RepoIdResolver,
    source: &AtUri<DefaultStr>,
    parsed: Record,
) {
    let Some(searchable) = SearchableRecord::try_from_record(parsed) else {
        return;
    };
    let Some(searchable) = searchable.normalize(resolver).await else {
        return;
    };
    search.upsert(searchable.to_search_doc(source)).await;
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn handle_frame<S: SearchSink + 'static>(
    frame: HydrantFrame,
    store: &EdgeStore,
    issue_states: &StateIndex<IssueStateKind>,
    pull_statuses: &StateIndex<PullStatusKind>,
    coverage: &CoverageWatch,
    search: &S,
    records: &dyn RecordStore,
    resolver: &RepoIdResolver,
    clock: &dyn Clock,
    now: UnixMicros,
) {
    let _ = clock;
    let ctx = PipelineCtx {
        resolver,
        store,
        issue_states,
        pull_statuses,
        coverage,
        records,
        search,
        shadow: None,
        buffer: None,
        knot_registry: None,
        knot_gate: None,
    };
    let pending = prepare_frame(frame, &ctx, now).await;
    let pending = resolve_pending(pending, &ctx).await;
    commit_pending(
        pending,
        store,
        issue_states,
        pull_statuses,
        coverage,
        search,
        records,
        resolver,
    )
    .await;
}

fn log_unknown_state_variant(outcome: ApplyOutcome, source: &AtUri<DefaultStr>) {
    if matches!(outcome, ApplyOutcome::UnknownVariant) {
        warn!(
            target: "bobbin_ingest::state_index",
            %source,
            "state record has unknown wire variant, skipping index update",
        );
    }
}

fn apply_delete_to_state_index(
    issue_states: &StateIndex<IssueStateKind>,
    pull_statuses: &StateIndex<PullStatusKind>,
    source: &AtUri<DefaultStr>,
    nsid: &Nsid<DefaultStr>,
) {
    match nsid.as_ref() {
        "sh.tangled.repo.issue" => issue_states.remove_entity(source),
        "sh.tangled.repo.pull" => pull_statuses.remove_entity(source),
        "sh.tangled.repo.issue.state" => issue_states.remove_source(source),
        "sh.tangled.repo.pull.status" => pull_statuses.remove_source(source),
        _ => {}
    }
}

fn promotion_signal(record: Option<&RecordFrame>, now: UnixMicros) -> PromotionSignal {
    PromotionSignal {
        rev_micros: record.map(|r| r.rev.timestamp()),
        now_micros: now.raw(),
        skew_micros: READY_SKEW.as_micros() as u64,
    }
}

fn cache_body(
    records: &dyn RecordStore,
    source: &AtUri<DefaultStr>,
    cid: Option<Cid<DefaultStr>>,
    bytes: Bytes,
) {
    match cid {
        Some(cid) => records.put(
            source.clone(),
            Arc::new(RecordBody {
                uri: source.clone(),
                cid,
                value: bytes,
            }),
        ),
        None => records.remove(source),
    }
}

async fn normalize_subjects(
    edges: Vec<Edge>,
    resolver: &RepoIdResolver,
    coverage: &CoverageWatch,
    shadow: Option<&WarmingShadowBuffer>,
) -> Vec<Edge> {
    let warming = shadow.is_some() && !coverage.snapshot().is_ready();
    futures::stream::iter(edges)
        .filter_map(|edge| async move {
            let Some(uri) = edge.subject.as_uri() else {
                return Some(edge);
            };
            let Some((owner, rkey)) = parse_repo_subject_uri(uri) else {
                return Some(edge);
            };
            if warming
                && let Some(shadow) = shadow
                && resolver.cached_resolution(&owner, &rkey).await.is_none()
            {
                shadow
                    .note_unresolved(owner.clone(), rkey.clone())
                    .await;
            }
            match resolver.resolve(&owner, &rkey).await {
                Resolution::Mapped(repo_did) => Some(Edge {
                    subject: SubjectRef::Did(repo_did),
                    ..edge
                }),
                Resolution::NoRepoDid => {
                    warn!(
                        target: "bobbin_ingest::normalize",
                        kind = %edge.kind,
                        owner = owner.as_ref(),
                        rkey = rkey.as_ref(),
                        source = edge.source.as_ref(),
                        "dropping edge: target repo has no repoDid, no canonical DID subject available",
                    );
                    None
                }
                Resolution::Unresolvable => {
                    warn!(
                        target: "bobbin_ingest::normalize",
                        kind = %edge.kind,
                        owner = owner.as_ref(),
                        rkey = rkey.as_ref(),
                        source = edge.source.as_ref(),
                        "dropping edge: repo unresolvable, rkey-form subject will not match bare-DID queries",
                    );
                    None
                }
            }
        })
        .collect()
        .await
}

fn parse_repo_subject_uri(uri: &AtUri<DefaultStr>) -> Option<(Did<DefaultStr>, Rkey<DefaultStr>)> {
    let collection = uri.collection()?;
    if collection.as_ref() != "sh.tangled.repo" {
        return None;
    }
    let AtIdentifier::Did(authority) = uri.authority() else {
        return None;
    };
    let rkey = uri.rkey()?;
    let owner = Did::new_owned(authority.as_ref()).ok()?;
    let rkey = Rkey::new_owned(rkey.as_ref()).ok()?;
    Some((owner, rkey))
}

fn build_source_uri(r: &RecordFrame) -> Result<AtUri<DefaultStr>, IngestError> {
    Ok(AtUri::from_parts_owned(
        r.did.as_ref(),
        r.collection.as_ref(),
        r.rkey.as_ref(),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobbin_edge_index::Coverage;
    use bobbin_record_lru::{CacheCapacity, LruRecordStore, NoopRecordStore, RecordStore};
    use bobbin_runtime::{OsEntropy, RuntimeHasher, SystemClock, TungsteniteWs};
    use bobbin_types::search::NoopSearchSink;
    use jacquard_common::types::nsid::Nsid;
    use jacquard_common::types::tid::Tid;
    use serde_json::json;

    const VALID_CID: &str = "bafyreieqygohnz2zqyvtvktbjpvhutphobcmbsnt4q5lc36ri7vpcmoz4i";

    fn did_subj(s: &str) -> SubjectRef {
        SubjectRef::Did(Did::new_owned(s).unwrap())
    }

    fn uri_subj(s: &str) -> SubjectRef {
        SubjectRef::Uri(AtUri::new_owned(s).unwrap())
    }

    fn rkey(s: &str) -> Rkey<DefaultStr> {
        Rkey::new_owned(s).unwrap()
    }

    #[allow(clippy::type_complexity)]
    fn fresh() -> (
        Arc<EdgeStore>,
        Arc<StateIndex<IssueStateKind>>,
        Arc<StateIndex<PullStatusKind>>,
        Arc<CoverageWatch>,
        Arc<RepoIdResolver>,
    ) {
        (
            Arc::new(EdgeStore::new(RuntimeHasher::default())),
            Arc::new(StateIndex::new(RuntimeHasher::default())),
            Arc::new(StateIndex::new(RuntimeHasher::default())),
            Arc::new(CoverageWatch::new()),
            Arc::new(RepoIdResolver::detached(RuntimeHasher::default())),
        )
    }

    fn now() -> UnixMicros {
        SystemClock::new().now_unix_micros()
    }

    fn sys_clock() -> SystemClock {
        SystemClock::new()
    }

    fn parse_frame(value: serde_json::Value) -> HydrantFrame {
        let text = serde_json::to_string(&value).expect("serialize fixture");
        serde_json::from_str(&text).expect("deserialize fixture")
    }

    fn fresh_tid() -> Tid {
        Tid::now_0()
    }

    #[tokio::test]
    async fn ignores_non_tangled_collections() {
        let (store, issue_states, pull_statuses, cov, resolver) = fresh();
        let frame: HydrantFrame = parse_frame(json!({
            "id": 1,
            "type": "record",
            "record": {
                "live": false,
                "did": "did:plc:nel",
                "rev": fresh_tid().as_str(),
                "collection": "app.bsky.feed.post",
                "rkey": "abcabcabcabcz",
                "action": "create",
                "record": {"$type": "app.bsky.feed.post", "text": "hi"}
            }
        }));
        handle_frame(
            frame,
            &store,
            &issue_states,
            &pull_statuses,
            &cov,
            &NoopSearchSink,
            &NoopRecordStore,
            &resolver,
            &sys_clock(),
            now(),
        )
        .await;
        assert_eq!(store.key_count(), 0);
        assert_eq!(cov.snapshot().events_processed(), 1);
    }

    #[tokio::test]
    async fn native_knot_member_skipped_legacy_indexed() {
        use bobbin_knot_ingest::{CapabilityGate, KnotClient, KnotRegistry};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.knot.version"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "version": "1.1.0",
                "capabilities": ["knot-acl"]
            })))
            .mount(&server)
            .await;
        let url = url::Url::parse(&server.uri()).unwrap();
        let native_host = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());

        let gate = CapabilityGate::new(
            KnotClient::with_default_http(true).unwrap(),
            Arc::new(SystemClock::new()),
            true,
            true,
        );
        assert!(gate.has_knot_acl(&KnotHostKey::new(&native_host)).await);

        let registry = KnotRegistry::new();
        let (store, issue_states, pull_statuses, cov, resolver) = fresh();
        let ctx = PipelineCtx {
            resolver: &resolver,
            store: &store,
            issue_states: &issue_states,
            pull_statuses: &pull_statuses,
            coverage: &cov,
            records: &NoopRecordStore,
            search: &NoopSearchSink,
            shadow: None,
            buffer: None,
            knot_registry: Some(&registry),
            knot_gate: Some(&gate),
        };

        let member_frame = |id: u64, rkey: &str, domain: &str| {
            parse_frame(json!({
                "id": id,
                "type": "record",
                "record": {
                    "live": false,
                    "did": "did:plc:akshay",
                    "rev": fresh_tid().as_str(),
                    "collection": "sh.tangled.knot.member",
                    "rkey": rkey,
                    "action": "create",
                    "record": {
                        "$type": "sh.tangled.knot.member",
                        "subject": "did:plc:boltless",
                        "domain": domain,
                        "createdAt": "2026-06-01T00:00:00Z"
                    }
                }
            }))
        };

        let native =
            prepare_frame(member_frame(1, "aaaaaaaaaaaaz", &native_host), &ctx, now()).await;
        assert!(
            matches!(native.op, PendingOp::Delete { .. }),
            "member record for a native knot must be dropped"
        );

        let legacy =
            prepare_frame(member_frame(2, "bbbbbbbbbbbbz", "legacy.knot"), &ctx, now()).await;
        assert!(
            matches!(legacy.op, PendingOp::Upsert(_)),
            "member record for a legacy knot must be ingested"
        );
        assert!(
            registry.hosts().contains(&KnotHostKey::new("legacy.knot")),
            "a member record seeds host discovery even before any repo is seen"
        );
        assert_eq!(
            registry
                .drain_legacy_members(&KnotHostKey::new("legacy.knot"))
                .len(),
            1,
            "legacy member edge is indexed for later purge once the knot upgrades"
        );
    }

    #[tokio::test]
    async fn create_then_delete_round_trips_a_star() {
        let (store, issue_states, pull_statuses, cov, resolver) = fresh();
        let create: HydrantFrame = parse_frame(json!({
            "id": 10,
            "type": "record",
            "record": {
                "live": false,
                "did": "did:plc:olaren",
                "rev": fresh_tid().as_str(),
                "collection": "sh.tangled.feed.star",
                "rkey": "abcabcabcabcz",
                "action": "create",
                "record": {
                    "$type": "sh.tangled.feed.star",
                    "createdAt": "2026-05-01T00:00:00Z",
                    "subjectDid": "did:plc:abalone"
                }
            }
        }));
        handle_frame(
            create,
            &store,
            &issue_states,
            &pull_statuses,
            &cov,
            &NoopSearchSink,
            &NoopRecordStore,
            &resolver,
            &sys_clock(),
            now(),
        )
        .await;
        let key = bobbin_types::ids::EdgeKey::new(
            Nsid::new_static("sh.tangled.feed.star").unwrap(),
            did_subj("did:plc:abalone"),
        );
        assert_eq!(store.count(&key), 1);

        let delete: HydrantFrame = parse_frame(json!({
            "id": 11,
            "type": "record",
            "record": {
                "live": false,
                "did": "did:plc:olaren",
                "rev": fresh_tid().as_str(),
                "collection": "sh.tangled.feed.star",
                "rkey": "abcabcabcabcz",
                "action": "delete",
                "record": null
            }
        }));
        handle_frame(
            delete,
            &store,
            &issue_states,
            &pull_statuses,
            &cov,
            &NoopSearchSink,
            &NoopRecordStore,
            &resolver,
            &sys_clock(),
            now(),
        )
        .await;
        assert_eq!(store.count(&key), 0);
    }

    #[tokio::test]
    async fn update_replaces_prior_edges() {
        let (store, issue_states, pull_statuses, cov, resolver) = fresh();
        let mk = |subject_did: &Did<DefaultStr>, id: u64| -> HydrantFrame {
            parse_frame(json!({
                "id": id,
                "type": "record",
                "record": {
                    "live": false,
                    "did": "did:plc:olaren",
                    "rev": fresh_tid().as_str(),
                    "collection": "sh.tangled.feed.star",
                    "rkey": "abcabcabcabcz",
                    "action": "update",
                    "record": {
                        "$type": "sh.tangled.feed.star",
                        "createdAt": "2026-05-01T00:00:00Z",
                        "subjectDid": subject_did.as_ref()
                    }
                }
            }))
        };
        handle_frame(
            mk(&Did::new_owned("did:plc:abalone").unwrap(), 1),
            &store,
            &issue_states,
            &pull_statuses,
            &cov,
            &NoopSearchSink,
            &NoopRecordStore,
            &resolver,
            &sys_clock(),
            now(),
        )
        .await;
        handle_frame(
            mk(&Did::new_owned("did:plc:uni").unwrap(), 2),
            &store,
            &issue_states,
            &pull_statuses,
            &cov,
            &NoopSearchSink,
            &NoopRecordStore,
            &resolver,
            &sys_clock(),
            now(),
        )
        .await;

        let kind = Nsid::new_static("sh.tangled.feed.star").unwrap();
        let old = bobbin_types::ids::EdgeKey::new(kind.clone(), did_subj("did:plc:abalone"));
        let new = bobbin_types::ids::EdgeKey::new(kind, did_subj("did:plc:uni"));
        assert_eq!(store.count(&old), 0);
        assert_eq!(store.count(&new), 1);
    }

    #[tokio::test]
    async fn prepare_frame_tags_regime_from_live_flag() {
        let (store, issue_states, pull_statuses, cov, resolver) = fresh();
        let search = NoopSearchSink;
        let records = NoopRecordStore;
        let ctx = PipelineCtx {
            resolver: &resolver,
            store: &store,
            issue_states: &issue_states,
            pull_statuses: &pull_statuses,
            coverage: &cov,
            records: &records,
            search: &search,
            shadow: None,
            buffer: None,
            knot_registry: None,
            knot_gate: None,
        };
        let mk = |live: bool| -> HydrantFrame {
            parse_frame(json!({
                "id": 1,
                "type": "record",
                "record": {
                    "live": live,
                    "did": "did:plc:olaren",
                    "rev": fresh_tid().as_str(),
                    "collection": "sh.tangled.feed.star",
                    "rkey": "abcabcabcabcz",
                    "action": "create",
                    "record": {
                        "$type": "sh.tangled.feed.star",
                        "createdAt": "2026-05-01T00:00:00Z",
                        "subjectDid": "did:plc:abalone"
                    }
                }
            }))
        };
        let live_pending = prepare_frame(mk(true), &ctx, now()).await;
        assert_eq!(live_pending.regime, Regime::Live);
        let replay_pending = prepare_frame(mk(false), &ctx, now()).await;
        assert_eq!(replay_pending.regime, Regime::Replay);

        let identity: HydrantFrame = parse_frame(json!({
            "id": 9,
            "type": "identity",
        }));
        let id_pending = prepare_frame(identity, &ctx, now()).await;
        assert_eq!(id_pending.regime, Regime::NonRecord);
    }

    #[tokio::test]
    async fn create_with_cid_warms_record_lru() {
        let (store, issue_states, pull_statuses, cov, resolver) = fresh();
        let lru = LruRecordStore::new(CacheCapacity::from_bytes(64 * 1024));
        let frame: HydrantFrame = parse_frame(json!({
            "id": 1,
            "type": "record",
            "record": {
                "live": false,
                "did": "did:plc:olaren",
                "rev": fresh_tid().as_str(),
                "collection": "sh.tangled.feed.star",
                "rkey": "abcabcabcabcz",
                "action": "create",
                "cid": VALID_CID,
                "record": {
                    "$type": "sh.tangled.feed.star",
                    "createdAt": "2026-05-01T00:00:00Z",
                    "subjectDid": "did:plc:abalone"
                }
            }
        }));
        let source =
            AtUri::new_owned("at://did:plc:olaren/sh.tangled.feed.star/abcabcabcabcz").unwrap();
        handle_frame(
            frame,
            &store,
            &issue_states,
            &pull_statuses,
            &cov,
            &NoopSearchSink,
            &lru,
            &resolver,
            &sys_clock(),
            now(),
        )
        .await;
        let cached = lru.get(&source).expect("hydrant cid must seed the lru");
        assert_eq!(cached.cid.as_ref(), VALID_CID);
        let parsed: serde_json::Value = serde_json::from_slice(&cached.value).unwrap();
        assert_eq!(
            parsed["subject"]["did"], "did:plc:abalone",
            "legacy wire is upgraded to canon shape before caching so downstream readers see canonical fields"
        );
    }

    #[tokio::test]
    async fn create_without_cid_clears_record_lru() {
        let (store, issue_states, pull_statuses, cov, resolver) = fresh();
        let lru = LruRecordStore::new(CacheCapacity::from_bytes(64 * 1024));
        let source =
            AtUri::new_owned("at://did:plc:olaren/sh.tangled.feed.star/abcabcabcabcz").unwrap();
        let cid: Cid<DefaultStr> = VALID_CID.parse().unwrap();
        lru.put(
            source.clone(),
            Arc::new(RecordBody {
                uri: source.clone(),
                cid,
                value: bytes::Bytes::from_static(b"{\"stale\":true}"),
            }),
        );
        let frame: HydrantFrame = parse_frame(json!({
            "id": 1,
            "type": "record",
            "record": {
                "live": false,
                "did": "did:plc:olaren",
                "rev": fresh_tid().as_str(),
                "collection": "sh.tangled.feed.star",
                "rkey": "abcabcabcabcz",
                "action": "update",
                "record": {
                    "$type": "sh.tangled.feed.star",
                    "createdAt": "2026-05-01T00:00:00Z",
                    "subjectDid": "did:plc:abalone"
                }
            }
        }));
        handle_frame(
            frame,
            &store,
            &issue_states,
            &pull_statuses,
            &cov,
            &NoopSearchSink,
            &lru,
            &resolver,
            &sys_clock(),
            now(),
        )
        .await;
        assert!(
            lru.get(&source).is_none(),
            "missing cid means we cannot trust the body, so the lru must be cleared",
        );
    }

    #[tokio::test]
    async fn delete_evicts_record_lru() {
        let (store, issue_states, pull_statuses, cov, resolver) = fresh();
        let lru = LruRecordStore::new(CacheCapacity::from_bytes(64 * 1024));
        let source =
            AtUri::new_owned("at://did:plc:olaren/sh.tangled.feed.star/abcabcabcabcz").unwrap();
        let cid: Cid<DefaultStr> = VALID_CID.parse().unwrap();
        lru.put(
            source.clone(),
            Arc::new(RecordBody {
                uri: source.clone(),
                cid,
                value: bytes::Bytes::from_static(b"{}"),
            }),
        );
        let frame: HydrantFrame = parse_frame(json!({
            "id": 1,
            "type": "record",
            "record": {
                "live": false,
                "did": "did:plc:olaren",
                "rev": fresh_tid().as_str(),
                "collection": "sh.tangled.feed.star",
                "rkey": "abcabcabcabcz",
                "action": "delete",
                "record": null
            }
        }));
        handle_frame(
            frame,
            &store,
            &issue_states,
            &pull_statuses,
            &cov,
            &NoopSearchSink,
            &lru,
            &resolver,
            &sys_clock(),
            now(),
        )
        .await;
        assert!(lru.get(&source).is_none());
    }

    #[tokio::test]
    async fn live_recent_event_promotes_coverage_to_ready() {
        let (store, issue_states, pull_statuses, cov, resolver) = fresh();
        assert!(!cov.snapshot().is_ready());
        let frame: HydrantFrame = parse_frame(json!({
            "id": 99,
            "type": "record",
            "record": {
                "live": true,
                "did": "did:plc:olaren",
                "rev": fresh_tid().as_str(),
                "collection": "sh.tangled.feed.star",
                "rkey": "abcabcabcabcz",
                "action": "create",
                "record": {
                    "$type": "sh.tangled.feed.star",
                    "createdAt": "2026-05-01T00:00:00Z",
                    "subjectDid": "did:plc:abalone"
                }
            }
        }));
        handle_frame(
            frame,
            &store,
            &issue_states,
            &pull_statuses,
            &cov,
            &NoopSearchSink,
            &NoopRecordStore,
            &resolver,
            &sys_clock(),
            now(),
        )
        .await;
        assert!(cov.snapshot().is_ready());
        assert_eq!(cov.snapshot().last_cursor(), HydrantCursor::new(99));
    }

    #[tokio::test]
    async fn live_but_stale_rev_does_not_promote() {
        let (store, issue_states, pull_statuses, cov, resolver) = fresh();
        let stale_tid = Tid::from_time(1_000_000, 0);
        let frame: HydrantFrame = parse_frame(json!({
            "id": 7,
            "type": "record",
            "record": {
                "live": true,
                "did": "did:plc:olaren",
                "rev": stale_tid.as_str(),
                "collection": "sh.tangled.feed.star",
                "rkey": "abcabcabcabcz",
                "action": "create",
                "record": {
                    "$type": "sh.tangled.feed.star",
                    "createdAt": "2026-05-01T00:00:00Z",
                    "subjectDid": "did:plc:abalone"
                }
            }
        }));
        handle_frame(
            frame,
            &store,
            &issue_states,
            &pull_statuses,
            &cov,
            &NoopSearchSink,
            &NoopRecordStore,
            &resolver,
            &sys_clock(),
            now(),
        )
        .await;
        assert!(!cov.snapshot().is_ready());
        assert!(matches!(cov.snapshot(), Coverage::Warming { .. }));
    }

    #[test]
    fn classify_consumer_too_slow_frame_returns_typed_variant() {
        let text = r#"{"type":"error","error":"ConsumerTooSlow","message":"stream socket send blocked for at least 30 seconds"}"#;
        match classify_text_frame(text) {
            Err(IngestError::ConsumerTooSlow { message }) => {
                assert!(
                    message.contains("30 seconds"),
                    "message field preserved verbatim, got: {message}"
                );
            }
            other => panic!("expected ConsumerTooSlow variant, got: {other:?}"),
        }
    }

    #[test]
    fn classify_unknown_hydrant_error_falls_back_to_generic_variant() {
        let text = r#"{"type":"error","error":"NewFutureCode","message":"some new failure mode"}"#;
        match classify_text_frame(text) {
            Err(IngestError::HydrantStream { code, message }) => {
                assert_eq!(code, "NewFutureCode");
                assert_eq!(message, "some new failure mode");
            }
            other => panic!("expected HydrantStream variant, got: {other:?}"),
        }
    }

    #[test]
    fn classify_error_frame_without_message_uses_empty_string() {
        let text = r#"{"type":"error","error":"ConsumerTooSlow"}"#;
        match classify_text_frame(text) {
            Err(IngestError::ConsumerTooSlow { message }) => assert!(message.is_empty()),
            other => panic!("expected ConsumerTooSlow with empty message, got: {other:?}"),
        }
    }

    #[test]
    fn classify_normal_record_frame_unchanged() {
        let text = r#"{"id":42,"type":"record"}"#;
        let frame = classify_text_frame(text).expect("normal record frame must parse");
        assert_eq!(frame.id, 42);
        assert_eq!(frame.kind, FrameKind::Record);
    }

    #[test]
    fn classify_garbage_object_returns_decode_error() {
        let text = r#"{"random":"object","without":"required fields"}"#;
        match classify_text_frame(text) {
            Err(IngestError::Decode(_)) => {}
            other => panic!("expected Decode error, got: {other:?}"),
        }
    }

    struct ScriptedWsStream {
        messages: std::collections::VecDeque<Result<WsMessage, NetworkError>>,
    }

    impl WsStream for ScriptedWsStream {
        fn next<'a>(&'a mut self) -> bobbin_runtime::WsMessageFuture<'a> {
            let msg = self.messages.pop_front();
            Box::pin(async move { msg })
        }
    }

    #[tokio::test]
    async fn reader_loop_surfaces_consumer_too_slow_from_error_frame() {
        let mut messages = std::collections::VecDeque::new();
        messages.push_back(Ok(WsMessage::Text(
            r#"{"type":"error","error":"ConsumerTooSlow","message":"stream delivery blocked"}"#
                .to_owned(),
        )));
        let stream: Box<dyn WsStream> = Box::new(ScriptedWsStream { messages });
        let (frame_tx, _frame_rx) = tokio::sync::mpsc::channel::<HydrantFrame>(FRAME_CHANNEL_DEPTH);
        let (control_tx, _control_rx) =
            tokio::sync::mpsc::channel::<WsEvent>(CONTROL_CHANNEL_DEPTH);
        let cancel = CancellationToken::new();

        let end = reader_loop(stream, frame_tx, control_tx, cancel).await;

        match end.error {
            Some(IngestError::ConsumerTooSlow { message }) => {
                assert_eq!(message, "stream delivery blocked");
            }
            other => panic!("expected ConsumerTooSlow SessionEnd error, got: {other:?}"),
        }
        assert_eq!(end.outcome, SessionOutcome::Empty);
    }

    #[tokio::test]
    async fn reader_loop_surfaces_unknown_hydrant_error_distinctly() {
        let mut messages = std::collections::VecDeque::new();
        messages.push_back(Ok(WsMessage::Text(
            r#"{"type":"error","error":"NewFutureCode","message":"new mode"}"#.to_owned(),
        )));
        let stream: Box<dyn WsStream> = Box::new(ScriptedWsStream { messages });
        let (frame_tx, _frame_rx) = tokio::sync::mpsc::channel::<HydrantFrame>(FRAME_CHANNEL_DEPTH);
        let (control_tx, _control_rx) =
            tokio::sync::mpsc::channel::<WsEvent>(CONTROL_CHANNEL_DEPTH);
        let cancel = CancellationToken::new();

        let end = reader_loop(stream, frame_tx, control_tx, cancel).await;

        match end.error {
            Some(IngestError::HydrantStream { code, message }) => {
                assert_eq!(code, "NewFutureCode");
                assert_eq!(message, "new mode");
            }
            other => panic!("expected HydrantStream SessionEnd error, got: {other:?}"),
        }
    }

    struct ChannelWsStream {
        rx: tokio::sync::mpsc::Receiver<Result<WsMessage, NetworkError>>,
    }

    impl WsStream for ChannelWsStream {
        fn next<'a>(&'a mut self) -> bobbin_runtime::WsMessageFuture<'a> {
            Box::pin(async move { self.rx.recv().await })
        }
    }

    fn star_frame_text(id: u64, rkey: &Rkey<DefaultStr>) -> String {
        json!({
            "id": id,
            "type": "record",
            "record": {
                "live": false,
                "did": "did:plc:olaren",
                "rev": fresh_tid().as_str(),
                "collection": "sh.tangled.feed.star",
                "rkey": rkey.as_ref(),
                "action": "create",
                "record": {
                    "$type": "sh.tangled.feed.star",
                    "createdAt": "2026-05-01T00:00:00Z",
                    "subjectDid": "did:plc:abalone"
                }
            }
        })
        .to_string()
    }

    #[tokio::test]
    async fn pong_forwards_promptly_when_frame_channel_is_full() {
        let (ws_tx, ws_rx) = tokio::sync::mpsc::channel::<Result<WsMessage, NetworkError>>(8);
        let stream: Box<dyn WsStream> = Box::new(ChannelWsStream { rx: ws_rx });
        let (frame_tx, frame_rx) = tokio::sync::mpsc::channel::<HydrantFrame>(1);
        let (control_tx, mut control_rx) =
            tokio::sync::mpsc::channel::<WsEvent>(CONTROL_CHANNEL_DEPTH);
        let cancel = CancellationToken::new();

        let prefill: HydrantFrame = parse_frame(json!({
            "id": 0,
            "type": "record",
            "record": {
                "live": false,
                "did": "did:plc:olaren",
                "rev": fresh_tid().as_str(),
                "collection": "sh.tangled.feed.star",
                "rkey": "prefilrkey001",
                "action": "create",
                "record": {
                    "$type": "sh.tangled.feed.star",
                    "createdAt": "2026-05-01T00:00:00Z",
                    "subjectDid": "did:plc:abalone"
                }
            }
        }));
        frame_tx
            .try_send(prefill)
            .expect("depth-1 frame channel must accept the prefill");

        let reader_handle = tokio::spawn(reader_loop(
            stream,
            frame_tx.clone(),
            control_tx.clone(),
            cancel.clone(),
        ));

        ws_tx
            .send(Ok(WsMessage::Text(star_frame_text(
                1,
                &rkey("starrkeyaa001"),
            ))))
            .await
            .unwrap();
        ws_tx.send(Ok(WsMessage::Pong(Bytes::new()))).await.unwrap();

        let pong_event = tokio::time::timeout(Duration::from_millis(500), control_rx.recv())
            .await
            .expect("pong must surface inside 500ms even when frame_tx is saturated. A reader blocked on a frame send would never poll the next ws message")
            .expect("control_tx was not closed");
        assert!(
            matches!(pong_event, WsEvent::IncomingPong),
            "first control event must be the pong, not a held text",
        );

        let too_slow =
            "{\"type\":\"error\",\"error\":\"ConsumerTooSlow\",\"message\":\"saturated\"}"
                .to_string();
        ws_tx.send(Ok(WsMessage::Text(too_slow))).await.unwrap();

        let end = tokio::time::timeout(Duration::from_secs(1), reader_handle)
            .await
            .expect("reader must exit promptly once ConsumerTooSlow is read")
            .expect("reader task must not panic");
        match end.error {
            Some(IngestError::ConsumerTooSlow { message }) => assert_eq!(message, "saturated"),
            other => panic!("expected ConsumerTooSlow disconnect, got: {other:?}"),
        }

        drop(frame_rx);
    }

    #[tokio::test]
    async fn reader_drains_held_frames_once_processor_catches_up() {
        let (ws_tx, ws_rx) = tokio::sync::mpsc::channel::<Result<WsMessage, NetworkError>>(8);
        let stream: Box<dyn WsStream> = Box::new(ChannelWsStream { rx: ws_rx });
        let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel::<HydrantFrame>(1);
        let (control_tx, _control_rx) =
            tokio::sync::mpsc::channel::<WsEvent>(CONTROL_CHANNEL_DEPTH);
        let cancel = CancellationToken::new();

        let prefill: HydrantFrame = parse_frame(json!({
            "id": 0,
            "type": "record",
            "record": {
                "live": false,
                "did": "did:plc:olaren",
                "rev": fresh_tid().as_str(),
                "collection": "sh.tangled.feed.star",
                "rkey": "prefilrkey001",
                "action": "create",
                "record": {
                    "$type": "sh.tangled.feed.star",
                    "createdAt": "2026-05-01T00:00:00Z",
                    "subjectDid": "did:plc:abalone"
                }
            }
        }));
        frame_tx.try_send(prefill).unwrap();

        let reader_handle = tokio::spawn(reader_loop(
            stream,
            frame_tx.clone(),
            control_tx,
            cancel.clone(),
        ));

        ws_tx
            .send(Ok(WsMessage::Text(star_frame_text(
                1,
                &rkey("heldrkeyaa001"),
            ))))
            .await
            .unwrap();
        ws_tx
            .send(Ok(WsMessage::Text(star_frame_text(
                2,
                &rkey("heldrkeyaa002"),
            ))))
            .await
            .unwrap();

        let _drained_prefill = frame_rx.recv().await.expect("prefilled frame drains");
        let first = tokio::time::timeout(Duration::from_millis(500), frame_rx.recv())
            .await
            .expect("first held frame must reach frame_rx after slot opens")
            .expect("frame_tx still open");
        assert_eq!(first.id, 1);
        let second = tokio::time::timeout(Duration::from_millis(500), frame_rx.recv())
            .await
            .expect("second held frame must reach frame_rx after slot opens")
            .expect("frame_tx still open");
        assert_eq!(second.id, 2);

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), reader_handle)
            .await
            .expect("reader must stop after cancel");
    }

    #[test]
    fn first_connect_uses_configured_start_cursor() {
        let start = HydrantCursor::new(42);
        assert_eq!(next_connect_cursor(Coverage::default(), start), start);
    }

    #[test]
    fn reconnect_resumes_strictly_after_last_seen() {
        let snap = Coverage::default().advance(HydrantCursor::new(7));
        assert_eq!(
            next_connect_cursor(snap, HydrantCursor::new(0)),
            HydrantCursor::new(8),
        );
    }

    #[test]
    fn reconnect_overrides_configured_start() {
        let snap = Coverage::default().advance(HydrantCursor::new(100));
        assert_eq!(
            next_connect_cursor(snap, HydrantCursor::new(50)),
            HydrantCursor::new(101),
        );
    }

    #[test]
    fn first_connect_uses_start_even_when_first_frame_id_would_be_zero() {
        let start = HydrantCursor::new(7);
        let snap = Coverage::default();
        assert_eq!(snap.last_cursor(), HydrantCursor::new(0));
        assert_eq!(snap.events_processed(), 0);
        assert_eq!(next_connect_cursor(snap, start), start);
    }

    #[test]
    fn reconnect_after_processing_id_zero_advances_to_one() {
        let snap = Coverage::default().advance(HydrantCursor::new(0));
        assert_eq!(snap.events_processed(), 1);
        assert_eq!(
            next_connect_cursor(snap, HydrantCursor::new(99)),
            HydrantCursor::new(1),
            "events_processed disambiguates 'never seen' from 'saw id 0'",
        );
    }

    #[tokio::test]
    async fn identity_frame_advances_cursor_only() {
        let (store, issue_states, pull_statuses, cov, resolver) = fresh();
        let frame: HydrantFrame = parse_frame(json!({
            "id": 5,
            "type": "identity",
            "identity": {
                "did": "did:plc:olaren",
                "handle": "olaren.dev"
            }
        }));
        handle_frame(
            frame,
            &store,
            &issue_states,
            &pull_statuses,
            &cov,
            &NoopSearchSink,
            &NoopRecordStore,
            &resolver,
            &sys_clock(),
            now(),
        )
        .await;
        assert_eq!(store.key_count(), 0);
        assert_eq!(cov.snapshot().last_cursor(), HydrantCursor::new(5));
        assert!(!cov.snapshot().is_ready());
    }

    #[tokio::test]
    async fn account_frame_advances_cursor_only() {
        let (store, issue_states, pull_statuses, cov, resolver) = fresh();
        let frame: HydrantFrame = parse_frame(json!({
            "id": 6,
            "type": "account",
            "account": {"did": "did:plc:olaren", "active": true}
        }));
        handle_frame(
            frame,
            &store,
            &issue_states,
            &pull_statuses,
            &cov,
            &NoopSearchSink,
            &NoopRecordStore,
            &resolver,
            &sys_clock(),
            now(),
        )
        .await;
        assert_eq!(store.key_count(), 0);
        assert_eq!(cov.snapshot().last_cursor(), HydrantCursor::new(6));
    }

    #[tokio::test]
    async fn unknown_frame_kind_advances_cursor_without_panic() {
        let (store, issue_states, pull_statuses, cov, resolver) = fresh();
        let frame: HydrantFrame = parse_frame(json!({"id": 8, "type": "future_event"}));
        assert_eq!(frame.kind, FrameKind::Other);
        handle_frame(
            frame,
            &store,
            &issue_states,
            &pull_statuses,
            &cov,
            &NoopSearchSink,
            &NoopRecordStore,
            &resolver,
            &sys_clock(),
            now(),
        )
        .await;
        assert_eq!(cov.snapshot().last_cursor(), HydrantCursor::new(8));
    }

    fn fresh_runtime(cancel: CancellationToken) -> IngestRuntime<NoopSearchSink> {
        IngestRuntime {
            store: Arc::new(EdgeStore::new(RuntimeHasher::default())),
            issue_states: Arc::new(StateIndex::new(RuntimeHasher::default())),
            pull_statuses: Arc::new(StateIndex::new(RuntimeHasher::default())),
            coverage: Arc::new(CoverageWatch::new()),
            search: Arc::new(NoopSearchSink),
            records: Arc::new(NoopRecordStore) as Arc<dyn RecordStore>,
            resolver: Arc::new(RepoIdResolver::detached(RuntimeHasher::default())),
            clock: Arc::new(SystemClock::new()),
            entropy: Arc::new(OsEntropy),
            ws: TungsteniteWs::shared(),
            cancel,
            disconnects: None,
            warming_shadow: None,
            warming_buffer: None,
            knot_registry: None,
            knot_gate: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_token_short_circuits_reconnect_sleep() {
        let cfg = IngestConfig::new(Url::parse("ws://127.0.0.1:1").unwrap());
        let cancel = CancellationToken::new();
        let runtime = fresh_runtime(cancel.clone());
        let task = tokio::spawn(async move { run(cfg, runtime).await });
        tokio::time::advance(Duration::from_millis(10)).await;
        cancel.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("ingest must stop within timeout once cancel fires");
        assert!(matches!(outcome, Ok(Ok(()))), "got {outcome:?}");
    }

    #[test]
    fn jittered_stays_within_one_quarter_of_base() {
        let base = Duration::from_secs(1);
        let cap = base + Duration::from_millis(250);
        let entropy = OsEntropy;
        (0..50).for_each(|_| {
            let j = jittered(base, &entropy);
            assert!(j >= base, "jitter must not undershoot");
            assert!(j <= cap, "jitter must not exceed +25%, got {:?}", j);
        });
    }

    #[tokio::test]
    async fn star_after_observed_repo_keys_on_repo_did() {
        let (store, issue_states, pull_statuses, cov, resolver) = fresh();
        let repo: HydrantFrame = parse_frame(json!({
            "id": 1,
            "type": "record",
            "record": {
                "live": false,
                "did": "did:plc:nel",
                "rev": fresh_tid().as_str(),
                "collection": "sh.tangled.repo",
                "rkey": "abcabcabcabcz",
                "action": "create",
                "record": {
                    "$type": "sh.tangled.repo",
                    "createdAt": "2026-05-01T00:00:00Z",
                    "knot": "oyster.cafe",
                    "name": "abalone",
                    "repoDid": "did:plc:abalone"
                }
            }
        }));
        handle_frame(
            repo,
            &store,
            &issue_states,
            &pull_statuses,
            &cov,
            &NoopSearchSink,
            &NoopRecordStore,
            &resolver,
            &sys_clock(),
            now(),
        )
        .await;

        let star: HydrantFrame = parse_frame(json!({
            "id": 2,
            "type": "record",
            "record": {
                "live": false,
                "did": "did:plc:olaren",
                "rev": fresh_tid().as_str(),
                "collection": "sh.tangled.feed.star",
                "rkey": "abcabcabcabcz",
                "action": "create",
                "record": {
                    "$type": "sh.tangled.feed.star",
                    "createdAt": "2026-05-01T00:00:00Z",
                    "subject": "at://did:plc:nel/sh.tangled.repo/abcabcabcabcz"
                }
            }
        }));
        handle_frame(
            star,
            &store,
            &issue_states,
            &pull_statuses,
            &cov,
            &NoopSearchSink,
            &NoopRecordStore,
            &resolver,
            &sys_clock(),
            now(),
        )
        .await;

        let nsid = Nsid::new_static("sh.tangled.feed.star").unwrap();
        let repo_keyed = bobbin_types::ids::EdgeKey::new(nsid.clone(), did_subj("did:plc:abalone"));
        let owner_keyed = bobbin_types::ids::EdgeKey::new(nsid, did_subj("did:plc:nel"));
        assert_eq!(
            store.count(&repo_keyed),
            1,
            "star should be keyed on repoDID once the repo is observed"
        );
        assert_eq!(
            store.count(&owner_keyed),
            0,
            "owner DID should not collect the edge"
        );
    }

    #[tokio::test]
    async fn unresolvable_repo_subject_drops_edge() {
        let (store, issue_states, pull_statuses, cov, resolver) = fresh();
        let star: HydrantFrame = parse_frame(json!({
            "id": 1,
            "type": "record",
            "record": {
                "live": false,
                "did": "did:plc:olaren",
                "rev": fresh_tid().as_str(),
                "collection": "sh.tangled.feed.star",
                "rkey": "abcabcabcabcz",
                "action": "create",
                "record": {
                    "$type": "sh.tangled.feed.star",
                    "createdAt": "2026-05-01T00:00:00Z",
                    "subject": "at://did:plc:nel/sh.tangled.repo/abcabcabcabcz"
                }
            }
        }));
        handle_frame(
            star,
            &store,
            &issue_states,
            &pull_statuses,
            &cov,
            &NoopSearchSink,
            &NoopRecordStore,
            &resolver,
            &sys_clock(),
            now(),
        )
        .await;

        let nsid = Nsid::new_static("sh.tangled.feed.star").unwrap();
        let owner_keyed = bobbin_types::ids::EdgeKey::new(nsid.clone(), did_subj("did:plc:nel"));
        let uri_keyed = bobbin_types::ids::EdgeKey::new(
            nsid,
            uri_subj("at://did:plc:nel/sh.tangled.repo/abcabcabcabcz"),
        );
        assert_eq!(
            store.count(&owner_keyed),
            0,
            "must not silently misfile under the authoring DID",
        );
        assert_eq!(
            store.count(&uri_keyed),
            0,
            "unresolvable rkey-form subject must drop the edge; keeping it would index against a key that never matches bare-DID queries",
        );
    }

    #[tokio::test]
    async fn repo_without_repo_did_drops_edge() {
        let (store, issue_states, pull_statuses, cov, resolver) = fresh();
        let repo: HydrantFrame = parse_frame(json!({
            "id": 1,
            "type": "record",
            "record": {
                "live": false,
                "did": "did:plc:nel",
                "rev": fresh_tid().as_str(),
                "collection": "sh.tangled.repo",
                "rkey": "abcabcabcabcz",
                "action": "create",
                "record": {
                    "$type": "sh.tangled.repo",
                    "createdAt": "2026-05-01T00:00:00Z",
                    "knot": "oyster.cafe",
                    "name": "abalone"
                }
            }
        }));
        handle_frame(
            repo,
            &store,
            &issue_states,
            &pull_statuses,
            &cov,
            &NoopSearchSink,
            &NoopRecordStore,
            &resolver,
            &sys_clock(),
            now(),
        )
        .await;

        let star: HydrantFrame = parse_frame(json!({
            "id": 2,
            "type": "record",
            "record": {
                "live": false,
                "did": "did:plc:olaren",
                "rev": fresh_tid().as_str(),
                "collection": "sh.tangled.feed.star",
                "rkey": "abcabcabcabcz",
                "action": "create",
                "record": {
                    "$type": "sh.tangled.feed.star",
                    "createdAt": "2026-05-01T00:00:00Z",
                    "subject": "at://did:plc:nel/sh.tangled.repo/abcabcabcabcz"
                }
            }
        }));
        handle_frame(
            star,
            &store,
            &issue_states,
            &pull_statuses,
            &cov,
            &NoopSearchSink,
            &NoopRecordStore,
            &resolver,
            &sys_clock(),
            now(),
        )
        .await;

        let nsid = Nsid::new_static("sh.tangled.feed.star").unwrap();
        let uri_keyed = bobbin_types::ids::EdgeKey::new(
            nsid.clone(),
            uri_subj("at://did:plc:nel/sh.tangled.repo/abcabcabcabcz"),
        );
        let owner_keyed = bobbin_types::ids::EdgeKey::new(nsid, did_subj("did:plc:nel"));
        assert_eq!(
            store.count(&uri_keyed),
            0,
            "no canonical DID exists for a repo without repoDID, so the edge must be dropped",
        );
        assert_eq!(
            store.count(&owner_keyed),
            0,
            "the authoring DID is not the canonical repo identity",
        );
    }

    #[tokio::test]
    async fn explicit_subject_did_skips_normalization() {
        let (store, issue_states, pull_statuses, cov, resolver) = fresh();
        let star: HydrantFrame = parse_frame(json!({
            "id": 1,
            "type": "record",
            "record": {
                "live": false,
                "did": "did:plc:olaren",
                "rev": fresh_tid().as_str(),
                "collection": "sh.tangled.feed.star",
                "rkey": "abcabcabcabcz",
                "action": "create",
                "record": {
                    "$type": "sh.tangled.feed.star",
                    "createdAt": "2026-05-01T00:00:00Z",
                    "subjectDid": "did:plc:abalone"
                }
            }
        }));
        handle_frame(
            star,
            &store,
            &issue_states,
            &pull_statuses,
            &cov,
            &NoopSearchSink,
            &NoopRecordStore,
            &resolver,
            &sys_clock(),
            now(),
        )
        .await;
        let key = bobbin_types::ids::EdgeKey::new(
            Nsid::new_static("sh.tangled.feed.star").unwrap(),
            did_subj("did:plc:abalone"),
        );
        assert_eq!(store.count(&key), 1);
    }

    #[tokio::test]
    async fn issue_with_repo_uri_resolves_to_repo_did() {
        let (store, issue_states, pull_statuses, cov, resolver) = fresh();
        resolver
            .observe(
                Did::new_owned("did:plc:nel").unwrap(),
                Rkey::new_owned("abcabcabcabcz").unwrap(),
                Some(Did::new_owned("did:plc:abalone").unwrap()),
            )
            .await;
        let issue: HydrantFrame = parse_frame(json!({
            "id": 1,
            "type": "record",
            "record": {
                "live": false,
                "did": "did:plc:olaren",
                "rev": fresh_tid().as_str(),
                "collection": "sh.tangled.repo.issue",
                "rkey": "abcabcabcabcz",
                "action": "create",
                "record": {
                    "$type": "sh.tangled.repo.issue",
                    "createdAt": "2026-05-01T00:00:00Z",
                    "title": "bug",
                    "repo": "at://did:plc:nel/sh.tangled.repo/abcabcabcabcz"
                }
            }
        }));
        handle_frame(
            issue,
            &store,
            &issue_states,
            &pull_statuses,
            &cov,
            &NoopSearchSink,
            &NoopRecordStore,
            &resolver,
            &sys_clock(),
            now(),
        )
        .await;
        let key = bobbin_types::ids::EdgeKey::new(
            Nsid::new_static("sh.tangled.repo.issue").unwrap(),
            did_subj("did:plc:abalone"),
        );
        assert_eq!(store.count(&key), 1);
    }

    #[derive(Default)]
    struct RecordingSearchSink {
        docs: tokio::sync::Mutex<Vec<bobbin_types::search::SearchDoc>>,
    }

    impl SearchSink for RecordingSearchSink {
        async fn upsert(&self, doc: bobbin_types::search::SearchDoc) {
            self.docs.lock().await.push(doc);
        }
        async fn remove(&self, _uri: &AtUri<DefaultStr>) {}
    }

    #[tokio::test]
    async fn search_index_hydrates_repo_did_via_resolver() {
        let (store, issue_states, pull_statuses, cov, resolver) = fresh();
        resolver
            .observe(
                Did::new_owned("did:plc:nel").unwrap(),
                Rkey::new_owned("abcabcabcabcz").unwrap(),
                Some(Did::new_owned("did:plc:abalone").unwrap()),
            )
            .await;
        let search = RecordingSearchSink::default();
        let issue: HydrantFrame = parse_frame(json!({
            "id": 1,
            "type": "record",
            "record": {
                "live": false,
                "did": "did:plc:olaren",
                "rev": fresh_tid().as_str(),
                "collection": "sh.tangled.repo.issue",
                "rkey": "abcabcabcabcz",
                "action": "create",
                "record": {
                    "$type": "sh.tangled.repo.issue",
                    "createdAt": "2026-05-01T00:00:00Z",
                    "title": "bug",
                    "repo": "at://did:plc:nel/sh.tangled.repo/abcabcabcabcz"
                }
            }
        }));
        handle_frame(
            issue,
            &store,
            &issue_states,
            &pull_statuses,
            &cov,
            &search,
            &NoopRecordStore,
            &resolver,
            &sys_clock(),
            now(),
        )
        .await;
        let docs = search.docs.lock().await;
        assert_eq!(docs.len(), 1, "issue should produce one search doc");
        assert_eq!(
            docs[0].repo,
            Some(Did::new_owned("did:plc:abalone").unwrap()),
            "search doc repo field must be resolved from the observed repo, not left as None",
        );
    }

    #[tokio::test]
    async fn delete_repo_record_evicts_resolver_cache() {
        let (store, issue_states, pull_statuses, cov, resolver) = fresh();
        let owner = Did::new_owned("did:plc:nel").unwrap();
        let rkey = Rkey::new_owned("abcabcabcabcz").unwrap();
        resolver
            .observe(
                owner.clone(),
                rkey.clone(),
                Some(Did::new_owned("did:plc:abalone").unwrap()),
            )
            .await;
        assert!(
            resolver.cached_resolution(&owner, &rkey).await.is_some(),
            "observe must seed the cache",
        );
        let delete: HydrantFrame = parse_frame(json!({
            "id": 1,
            "type": "record",
            "record": {
                "live": false,
                "did": owner.as_ref(),
                "rev": fresh_tid().as_str(),
                "collection": "sh.tangled.repo",
                "rkey": rkey.as_ref(),
                "action": "delete"
            }
        }));
        handle_frame(
            delete,
            &store,
            &issue_states,
            &pull_statuses,
            &cov,
            &NoopSearchSink,
            &NoopRecordStore,
            &resolver,
            &sys_clock(),
            now(),
        )
        .await;
        assert!(
            resolver.cached_resolution(&owner, &rkey).await.is_none(),
            "deleting the repo record must clear the resolver cache so future observes are not blocked by a stale Authoritative entry",
        );
    }

    #[tokio::test]
    async fn cancel_short_circuits_a_hung_ws_connect() {
        let _listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind sink listener");
        let port = _listener.local_addr().expect("local addr").port();
        let cfg =
            IngestConfig::new(Url::parse(&format!("ws://127.0.0.1:{port}")).expect("hydrant url"));
        let cancel = CancellationToken::new();
        let runtime = fresh_runtime(cancel.clone());
        let task = tokio::spawn(async move { run(cfg, runtime).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("cancel must short-circuit the hung ws connect");
        assert!(matches!(outcome, Ok(Ok(()))), "got {outcome:?}");
    }

    struct CloseOnConnectTransport {
        used: std::sync::Mutex<bool>,
    }
    impl bobbin_runtime::WsTransport for CloseOnConnectTransport {
        fn connect(&self, _url: Url) -> bobbin_runtime::WsConnectFuture {
            let mut used = self.used.lock().unwrap();
            if *used {
                return Box::pin(async move {
                    Err(NetworkError::Connect("only one connect allowed".to_owned()))
                });
            }
            *used = true;
            Box::pin(async move {
                let mut q = std::collections::VecDeque::new();
                q.push_back(Ok(WsMessage::Close {
                    code: 1000,
                    reason: "bye".to_owned(),
                }));
                let stream: Box<dyn WsStream> = Box::new(ScriptedWsStream { messages: q });
                struct NoopSink;
                impl bobbin_runtime::WsSink for NoopSink {
                    fn send<'a>(&'a mut self, _m: WsMessage) -> bobbin_runtime::WsSendFuture<'a> {
                        Box::pin(async move { Ok(()) })
                    }
                }
                let sink: Box<dyn bobbin_runtime::WsSink> = Box::new(NoopSink);
                Ok(bobbin_runtime::WsConn { sink, stream })
            })
        }
    }

    #[tokio::test]
    async fn run_session_returns_after_remote_close_when_outer_cancel_unfired() {
        let cfg = IngestConfig::new(Url::parse("ws://127.0.0.1:1").unwrap());
        let cancel = CancellationToken::new();
        let mut runtime = fresh_runtime(cancel.clone());
        runtime.ws = Arc::new(CloseOnConnectTransport {
            used: std::sync::Mutex::new(false),
        });

        let res = tokio::time::timeout(
            Duration::from_secs(3),
            run_session(&cfg, HydrantCursor::new(0), &runtime),
        )
        .await;
        assert!(
            res.is_ok(),
            "run_session must return after a remote Close even when outer cancel never fires - regression for a session-scoped task hanging on the parent token",
        );
    }

    struct HangingSink;
    impl bobbin_runtime::WsSink for HangingSink {
        fn send<'a>(&'a mut self, _: WsMessage) -> bobbin_runtime::WsSendFuture<'a> {
            Box::pin(std::future::pending())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn timed_send_surfaces_send_timeout_when_sink_pends_forever() {
        let mut sink: Box<dyn bobbin_runtime::WsSink> = Box::new(HangingSink);
        let task =
            tokio::spawn(async move { timed_send(&mut sink, WsMessage::Ping(Bytes::new())).await });
        tokio::time::advance(SEND_TIMEOUT + Duration::from_secs(1)).await;
        let result = task.await.expect("task panicked");
        match result {
            Err(IngestError::SendTimeout(d)) => assert_eq!(d, SEND_TIMEOUT),
            other => panic!(
                "expected SendTimeout, got {other:?}. A bare ws_sink.send.await would hang forever on a half-dead socket and starve the writer's pong-deadline arm",
            ),
        }
    }

    struct OkSink;
    impl bobbin_runtime::WsSink for OkSink {
        fn send<'a>(&'a mut self, _: WsMessage) -> bobbin_runtime::WsSendFuture<'a> {
            Box::pin(async move { Ok(()) })
        }
    }

    #[tokio::test]
    async fn timed_send_returns_ok_when_sink_succeeds_promptly() {
        let mut sink: Box<dyn bobbin_runtime::WsSink> = Box::new(OkSink);
        let result = timed_send(&mut sink, WsMessage::Ping(Bytes::new())).await;
        assert!(matches!(result, Ok(())), "got {result:?}");
    }

    #[test]
    fn classify_error_frame_with_id_dispatches_as_error_not_unknown_kind() {
        let text = r#"{"id":42,"type":"error","error":"ConsumerTooSlow","message":"slow"}"#;
        match classify_text_frame(text) {
            Err(IngestError::ConsumerTooSlow { message }) => assert_eq!(message, "slow"),
            other => panic!(
                "type=\"error\" must dispatch to the error path even when id is present, got: {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn buffered_pipeline_preserves_cursor_order_under_resolve_latency_skew() {
        use bobbin_record_lru::RecordStore;
        use bobbin_types::record::RecordBody;
        use std::sync::Mutex;

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/xrpc/com.atproto.repo.getRecord"))
            .respond_with(
                wiremock::ResponseTemplate::new(404).set_delay(Duration::from_millis(150)),
            )
            .mount(&server)
            .await;

        let client = bobbin_slingshot_client::SlingshotClient::with_default_http(
            Url::parse(&server.uri()).unwrap(),
        )
        .unwrap();
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
        let resolver = Arc::new(RepoIdResolver::with_slingshot(
            client,
            clock.clone(),
            RuntimeHasher::default(),
        ));

        let owner = Did::new_owned("did:plc:nel").unwrap();
        let abalone = Did::new_owned("did:plc:abalone").unwrap();
        let fast_rkeys: [Rkey<DefaultStr>; 2] = [rkey("fastrkeyaa01"), rkey("fastrkeyaa02")];
        let slow_rkeys: [Rkey<DefaultStr>; 2] = [rkey("slowrkeyaa01"), rkey("slowrkeyaa02")];
        for r in &fast_rkeys {
            resolver
                .observe(owner.clone(), r.clone(), Some(abalone.clone()))
                .await;
        }

        #[derive(Default)]
        struct Capturing {
            urls: Mutex<Vec<AtUri<DefaultStr>>>,
        }
        impl RecordStore for Capturing {
            fn get(&self, _uri: &AtUri<DefaultStr>) -> Option<Arc<RecordBody>> {
                None
            }
            fn put(&self, uri: AtUri<DefaultStr>, _body: Arc<RecordBody>) {
                self.urls.lock().unwrap().push(uri);
            }
            fn remove(&self, _uri: &AtUri<DefaultStr>) {}
        }
        let capturing = Arc::new(Capturing::default());

        let runtime: IngestRuntime<NoopSearchSink> = IngestRuntime {
            store: Arc::new(EdgeStore::new(RuntimeHasher::default())),
            issue_states: Arc::new(StateIndex::new(RuntimeHasher::default())),
            pull_statuses: Arc::new(StateIndex::new(RuntimeHasher::default())),
            coverage: Arc::new(CoverageWatch::new()),
            search: Arc::new(NoopSearchSink),
            records: capturing.clone() as Arc<dyn RecordStore>,
            resolver,
            clock,
            entropy: Arc::new(OsEntropy),
            ws: TungsteniteWs::shared(),
            cancel: CancellationToken::new(),
            disconnects: None,
            warming_shadow: None,
            warming_buffer: None,
            knot_registry: None,
            knot_gate: None,
        };

        let parallelism = 4usize;
        let (frame_tx, frame_rx) = tokio::sync::mpsc::channel::<HydrantFrame>(64);
        let pipeline_rt = runtime.clone();
        let pipeline = tokio::spawn(async move {
            let prep_rt = pipeline_rt.clone();
            let resolve_rt = pipeline_rt.clone();
            let commit_rt = pipeline_rt;
            ReceiverStream::new(frame_rx)
                .then(move |frame| prep_stage(frame, prep_rt.clone()))
                .map(move |staged| resolve_stage(staged, resolve_rt.clone()))
                .buffered(parallelism)
                .for_each(move |staged| commit_stage(staged, commit_rt.clone(), parallelism))
                .await;
        });

        let mk = |id: u64, idx: u64, repo_rkey: &Rkey<DefaultStr>| -> HydrantFrame {
            parse_frame(json!({
                "id": id,
                "type": "record",
                "record": {
                    "live": false,
                    "did": "did:plc:olaren",
                    "rev": fresh_tid().as_str(),
                    "collection": "sh.tangled.feed.star",
                    "rkey": format!("starrkeya{idx:04}"),
                    "action": "create",
                    "cid": VALID_CID,
                    "record": {
                        "$type": "sh.tangled.feed.star",
                        "createdAt": "2026-05-01T00:00:00Z",
                        "subject": format!("at://did:plc:nel/sh.tangled.repo/{}", repo_rkey.as_ref()),
                    }
                }
            }))
        };

        frame_tx.send(mk(1, 1, &slow_rkeys[0])).await.unwrap();
        frame_tx.send(mk(2, 2, &fast_rkeys[0])).await.unwrap();
        frame_tx.send(mk(3, 3, &slow_rkeys[1])).await.unwrap();
        frame_tx.send(mk(4, 4, &fast_rkeys[1])).await.unwrap();
        drop(frame_tx);

        pipeline.await.unwrap();

        let captured = capturing.urls.lock().unwrap();
        let rkeys: Vec<String> = captured
            .iter()
            .filter_map(|uri| uri.rkey().map(|r| r.as_ref().to_owned()))
            .collect();
        assert_eq!(
            rkeys,
            vec![
                "starrkeya0001".to_owned(),
                "starrkeya0002".to_owned(),
                "starrkeya0003".to_owned(),
                "starrkeya0004".to_owned(),
            ],
            "buffered(N) must preserve cursor order even when resolves complete out of order, with ~150ms slow vs cache-hit fast as the latency skew here",
        );
        assert_eq!(runtime.coverage.snapshot().last_cursor().raw(), 4);
    }
}
