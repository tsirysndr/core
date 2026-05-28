use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bobbin_runtime::{
    Clock, HttpRequest, MemHttpBody, MemHttpResponder, MemHttpResponse, MemWsResponder,
    MemWsServerFuture, WsMessage,
};
use bytes::Bytes;
use http::StatusCode;
use jacquard_common::DefaultStr;
use jacquard_common::types::nsid::Nsid;
use tokio::sync::mpsc;
use url::Url;

use crate::report::{SimOutcome, SimReport};
use crate::workload::{Workload, WorkloadCtx, WorkloadHooks};
use crate::workloads::util::{format_rkey, format_tid, parse_repo_lookup};

const NAME: &str = "cancel-mid-hydration";
const REPO_COLLECTION: &str = "sh.tangled.repo";

fn repo_collection_nsid() -> Nsid<DefaultStr> {
    Nsid::new_static(REPO_COLLECTION).expect("REPO_COLLECTION literal is a valid NSID")
}

#[derive(Clone, Debug)]
pub struct CancelMidHydrationConfig {
    pub frames: usize,
    pub cancel_at_events: u64,
    pub slingshot_latency_ms: u64,
    pub frame_pace_us: u64,
}

impl Default for CancelMidHydrationConfig {
    fn default() -> Self {
        Self {
            frames: 200,
            cancel_at_events: 50,
            slingshot_latency_ms: 50,
            frame_pace_us: 100,
        }
    }
}

pub struct CancelMidHydration {
    config: CancelMidHydrationConfig,
}

impl CancelMidHydration {
    pub fn new(config: CancelMidHydrationConfig) -> Self {
        Self { config }
    }
}

impl Workload for CancelMidHydration {
    fn name(&self) -> &'static str {
        NAME
    }

    fn build(self: Box<Self>, ctx: WorkloadCtx) -> WorkloadHooks {
        let cfg = self.config.clone();
        let WorkloadCtx {
            seed,
            clock,
            coverage,
            store,
            cancel,
            ..
        } = ctx;

        let frames = build_frames(cfg.frames);
        let slingshot_calls = Arc::new(AtomicU64::new(0));

        let hydrant: Arc<dyn MemWsResponder> = Arc::new(PacedHydrant {
            frames: Mutex::new(Some(frames)),
            frame_pace: Duration::from_micros(cfg.frame_pace_us),
            clock: clock.clone(),
        });
        let slingshot: Arc<dyn MemHttpResponder> = Arc::new(LatentSlingshot {
            latency: Duration::from_millis(cfg.slingshot_latency_ms),
            slingshot_calls: slingshot_calls.clone(),
        });

        let cancel_at = cfg.cancel_at_events;
        let frames_total = cfg.frames as u64;
        let slingshot_calls_probe = slingshot_calls.clone();
        let script = Box::pin(async move {
            let started = clock.now_unix_micros();
            let mut rx = coverage.subscribe();
            let cancel_outcome = loop {
                let snap = *rx.borrow_and_update();
                if snap.events_processed() >= cancel_at {
                    break SimOutcome::Passed;
                }
                tokio::select! {
                    res = rx.changed() => match res {
                        Ok(()) => continue,
                        Err(_) => break SimOutcome::Failed,
                    },
                    _ = cancel.cancelled() => break SimOutcome::Failed,
                }
            };

            cancel.cancel();
            clock.sleep(Duration::from_millis(100)).await;
            let events_after_observe = coverage.snapshot().events_processed();
            let slingshot_after_observe = slingshot_calls_probe.load(Ordering::Relaxed);

            clock.sleep(Duration::from_secs(2)).await;
            let snap = coverage.snapshot();
            let virtual_runtime =
                Duration::from_micros(clock.now_unix_micros().raw().saturating_sub(started.raw()));
            let slingshot_after_drain = slingshot_calls_probe.load(Ordering::Relaxed);
            let post_observe_events = snap.events_processed().saturating_sub(events_after_observe);
            let post_observe_slingshot =
                slingshot_after_drain.saturating_sub(slingshot_after_observe);
            let failed = !matches!(cancel_outcome, SimOutcome::Passed)
                || post_observe_slingshot != 0
                || post_observe_events != 0
                || snap.events_processed() > frames_total
                || snap.events_processed() < cancel_at;
            let outcome = if failed {
                SimOutcome::Failed
            } else {
                SimOutcome::Passed
            };
            let failure_reason = match outcome {
                SimOutcome::Passed => None,
                SimOutcome::Failed => Some(format!(
                    "cancel-mid-hydration anomaly: events={} cancel_at={} \
                     events_after_observe={} post_observe_events={} \
                     slingshot_after_observe={} slingshot_after_drain={} \
                     post_observe_slingshot={}",
                    snap.events_processed(),
                    cancel_at,
                    events_after_observe,
                    post_observe_events,
                    slingshot_after_observe,
                    slingshot_after_drain,
                    post_observe_slingshot,
                )),
                SimOutcome::TimedOut => Some("timed out".into()),
            };
            SimReport {
                workload: NAME,
                seed,
                outcome,
                virtual_runtime,
                virtual_clock_end: clock.now_unix_micros(),
                events_processed: snap.events_processed(),
                last_cursor: snap.last_cursor().raw(),
                edge_count: store.key_count() as u64,
                resolver_hits: 0,
                resolver_misses: 0,
                consumer_too_slow_count: 0,
                disconnect_count: 0,
                last_disconnect: None,
                warming_shadow: Default::default(),
                warming_buffer: Default::default(),
                failure_reason,
            }
        });

        WorkloadHooks {
            slingshot,
            hydrant,
            script,
        }
    }
}

fn build_frames(count: usize) -> Vec<String> {
    (0..count)
        .map(|i| {
            let star_did = format!("did:plc:starer-{i}");
            let target_owner = format!("did:plc:owner-{i}");
            let target_rkey = format_rkey(i);
            serde_json::json!({
                "id": (i + 1) as u64,
                "type": "record",
                "record": {
                    "live": false,
                    "did": star_did,
                    "rev": format_tid(i),
                    "collection": "sh.tangled.feed.star",
                    "rkey": format_rkey(i + 1_000_000),
                    "action": "create",
                    "record": {
                        "$type": "sh.tangled.feed.star",
                        "createdAt": "2026-05-01T00:00:00Z",
                        "subject": format!("at://{target_owner}/{REPO_COLLECTION}/{target_rkey}")
                    }
                }
            })
            .to_string()
        })
        .collect()
}

struct PacedHydrant {
    frames: Mutex<Option<Vec<String>>>,
    frame_pace: Duration,
    clock: Arc<dyn Clock>,
}

impl MemWsResponder for PacedHydrant {
    fn spawn_server(
        &self,
        _: Url,
        mut recv: mpsc::UnboundedReceiver<WsMessage>,
        send: mpsc::Sender<WsMessage>,
    ) -> MemWsServerFuture {
        let frames = self.frames.lock().unwrap().take().unwrap_or_default();
        let frame_pace = self.frame_pace;
        let clock = self.clock.clone();
        let pong_send = send.clone();
        Box::pin(async move {
            let pong_loop = async move {
                loop {
                    match recv.recv().await {
                        Some(WsMessage::Ping(payload)) => {
                            if pong_send.send(WsMessage::Pong(payload)).await.is_err() {
                                return;
                            }
                        }
                        Some(WsMessage::Close { .. }) | None => return,
                        Some(_) => {}
                    }
                }
            };
            let frame_emit = async move {
                for text in frames {
                    if !frame_pace.is_zero() {
                        clock.sleep(frame_pace).await;
                    }
                    if send.send(WsMessage::Text(text)).await.is_err() {
                        return;
                    }
                }
            };
            let _ = tokio::join!(pong_loop, frame_emit);
        })
    }
}

struct LatentSlingshot {
    latency: Duration,
    slingshot_calls: Arc<AtomicU64>,
}

impl MemHttpResponder for LatentSlingshot {
    fn respond(&self, request: &HttpRequest) -> MemHttpResponse {
        self.slingshot_calls.fetch_add(1, Ordering::Relaxed);
        let owner_rkey = parse_repo_lookup(&request.url, &repo_collection_nsid());
        let body = match owner_rkey {
            Some((owner, rkey)) => {
                let repo_did = owner.as_ref().replace("owner-", "repo-");
                serde_json::json!({
                    "uri": format!("at://{}/{REPO_COLLECTION}/{}", owner.as_ref(), rkey.as_ref()),
                    "cid": "bafyreieqygohnz2zqyvtvktbjpvhutphobcmbsnt4q5lc36ri7vpcmoz4i",
                    "value": {
                        "$type": REPO_COLLECTION,
                        "createdAt": "2026-05-01T00:00:00Z",
                        "knot": "oyster.cafe",
                        "name": "scallop",
                        "repoDid": repo_did,
                    }
                })
                .to_string()
            }
            None => {
                return MemHttpResponse {
                    latency: self.latency,
                    result: Ok(MemHttpBody::status_only(StatusCode::NOT_FOUND)),
                };
            }
        };
        MemHttpResponse {
            latency: self.latency,
            result: Ok(MemHttpBody::ok_json(Bytes::from(body))),
        }
    }
}
