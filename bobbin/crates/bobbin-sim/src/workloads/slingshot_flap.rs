use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bobbin_runtime::{
    Clock, HttpRequest, MemHttpBody, MemHttpResponder, MemHttpResponse, MemWsResponder,
    MemWsServerFuture, NetworkError, UnixMicros, WsMessage,
};
use bytes::Bytes;
use http::StatusCode;
use jacquard_common::DefaultStr;
use jacquard_common::types::nsid::Nsid;
use jacquard_common::types::tid::Tid;
use tokio::sync::mpsc;
use url::Url;

use crate::report::{SimOutcome, SimReport};
use crate::workload::{Workload, WorkloadCtx, WorkloadHooks};
use crate::workloads::util::{format_rkey, format_tid, parse_repo_lookup};

const NAME: &str = "slingshot-flap";
const REPO_COLLECTION: &str = "sh.tangled.repo";

fn repo_collection_nsid() -> Nsid<DefaultStr> {
    Nsid::new_static(REPO_COLLECTION).expect("REPO_COLLECTION literal is a valid NSID")
}

#[derive(Clone, Debug)]
pub struct SlingshotFlapConfig {
    pub cross_did_stars: usize,
    pub normal_latency_ms: u64,
    pub brownout_start_ms: u64,
    pub brownout_duration_ms: u64,
    pub brownout_latency_ms: u64,
    pub brownout_enabled: bool,
    pub hydrant_send_timeout_ms: u64,
    pub hydrant_frame_pace_us: u64,
    pub omit_target_repos: bool,
    pub emit_live_promotion_frame: bool,
}

impl Default for SlingshotFlapConfig {
    fn default() -> Self {
        Self {
            cross_did_stars: 200,
            normal_latency_ms: 2,
            brownout_start_ms: 1_000,
            brownout_duration_ms: 5_000,
            brownout_latency_ms: 200,
            brownout_enabled: true,
            hydrant_send_timeout_ms: 30_000,
            hydrant_frame_pace_us: 0,
            omit_target_repos: false,
            emit_live_promotion_frame: false,
        }
    }
}

pub struct SlingshotFlap {
    config: SlingshotFlapConfig,
}

impl SlingshotFlap {
    pub fn new(config: SlingshotFlapConfig) -> Self {
        Self { config }
    }
}

impl Workload for SlingshotFlap {
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
            consumer_too_slow_count,
            ..
        } = ctx;

        let star_count = cfg.cross_did_stars;
        let repo_count = if cfg.omit_target_repos { 0 } else { star_count };
        let live_count = if cfg.emit_live_promotion_frame { 1 } else { 0 };
        let total_frames = (star_count + repo_count + live_count) as u64;
        let target_owner_count = star_count;

        let started_unix_pre = clock.now_unix_micros();
        let frame_script = Arc::new(build_frame_script(&cfg, started_unix_pre));
        let session_count = Arc::new(AtomicU64::new(0));
        let hydrant: Arc<dyn MemWsResponder> = Arc::new(FlapHydrant {
            frames: frame_script,
            send_timeout: Duration::from_millis(cfg.hydrant_send_timeout_ms),
            frame_pace: Duration::from_micros(cfg.hydrant_frame_pace_us),
            consumer_too_slow: consumer_too_slow_count.clone(),
            session_count: session_count.clone(),
            clock: clock.clone(),
        });

        let started_unix = started_unix_pre;
        let slingshot: Arc<dyn MemHttpResponder> = Arc::new(FlapSlingshot {
            cfg: cfg.clone(),
            clock: clock.clone(),
            started_unix,
        });

        let cts_counter = consumer_too_slow_count.clone();
        let sessions_counter = session_count.clone();
        let script = Box::pin(async move {
            let started = started_unix;

            let mut rx = coverage.subscribe();
            let outcome = loop {
                let snap = *rx.borrow_and_update();
                if snap.events_processed() >= total_frames {
                    break SimOutcome::Passed;
                }
                if cts_counter.load(Ordering::Relaxed) > 0 {
                    break SimOutcome::Failed;
                }
                tokio::select! {
                    res = rx.changed() => match res {
                        Ok(()) => continue,
                        Err(_) => break SimOutcome::Failed,
                    },
                    _ = cancel.cancelled() => break SimOutcome::Failed,
                }
            };
            let snap = coverage.snapshot();
            let virtual_runtime =
                Duration::from_micros(clock.now_unix_micros().raw().saturating_sub(started.raw()));
            let cts = cts_counter.load(Ordering::Relaxed);
            let sessions = sessions_counter.load(Ordering::Relaxed);
            let failure_reason = match outcome {
                SimOutcome::Passed => None,
                SimOutcome::Failed if cts > 0 => Some(format!(
                    "ConsumerTooSlow fired {cts} times across {sessions} sessions; processed {}/{} events before disconnect",
                    snap.events_processed(),
                    total_frames,
                )),
                SimOutcome::Failed => Some(format!(
                    "only {}/{} events processed across {sessions} sessions (expected {} owners staged)",
                    snap.events_processed(),
                    total_frames,
                    target_owner_count,
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
                consumer_too_slow_count: cts,
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

fn build_frame_script(cfg: &SlingshotFlapConfig, started_unix: UnixMicros) -> Vec<(u64, String)> {
    let mut frames = Vec::with_capacity(cfg.cross_did_stars * 2 + 1);
    for i in 0..cfg.cross_did_stars {
        let id = (i + 1) as u64;
        let star_did = format!("did:plc:starer-{i}");
        let target_owner = format!("did:plc:owner-{i}");
        let target_rkey = format_rkey(i);
        let body = serde_json::json!({
            "id": id,
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
        .to_string();
        frames.push((id, body));
    }
    if !cfg.omit_target_repos {
        for i in 0..cfg.cross_did_stars {
            let id = (cfg.cross_did_stars + i + 1) as u64;
            let owner = format!("did:plc:owner-{i}");
            let repo_did = format!("did:plc:repo-{i}");
            let body = serde_json::json!({
                "id": id,
                "type": "record",
                "record": {
                    "live": false,
                    "did": owner,
                    "rev": format_tid(cfg.cross_did_stars + i),
                    "collection": REPO_COLLECTION,
                    "rkey": format_rkey(i),
                    "action": "create",
                    "record": {
                        "$type": REPO_COLLECTION,
                        "createdAt": "2026-05-01T00:00:00Z",
                        "knot": "oyster.cafe",
                        "name": format!("repo-{i}"),
                        "repoDid": repo_did,
                    }
                }
            })
            .to_string();
            frames.push((id, body));
        }
    }
    if cfg.emit_live_promotion_frame {
        let prior = cfg.cross_did_stars
            + if cfg.omit_target_repos {
                0
            } else {
                cfg.cross_did_stars
            };
        let id = (prior + 1) as u64;
        let promoter_name = format!("periwinkle-{prior}");
        let promoter_did = format!("did:plc:{promoter_name}");
        let promoter_rkey = format_rkey(prior + 1);
        let live_rev = Tid::from_time(started_unix.raw(), 0);
        let body = serde_json::json!({
            "id": id,
            "type": "record",
            "record": {
                "live": true,
                "did": promoter_did,
                "rev": live_rev.as_str(),
                "collection": REPO_COLLECTION,
                "rkey": promoter_rkey,
                "action": "create",
                "record": {
                    "$type": REPO_COLLECTION,
                    "createdAt": "2026-05-01T00:00:00Z",
                    "knot": "oyster.cafe",
                    "name": promoter_name,
                    "repoDid": promoter_did,
                }
            }
        })
        .to_string();
        frames.push((id, body));
    }
    frames
}

fn parse_cursor(url: &Url) -> u64 {
    url.query_pairs()
        .find_map(|(k, v)| (k == "cursor").then(|| v.parse().ok()).flatten())
        .unwrap_or(0)
}

struct FlapHydrant {
    frames: Arc<Vec<(u64, String)>>,
    send_timeout: Duration,
    frame_pace: Duration,
    consumer_too_slow: Arc<AtomicU64>,
    session_count: Arc<AtomicU64>,
    clock: Arc<dyn Clock>,
}

impl MemWsResponder for FlapHydrant {
    fn spawn_server(
        &self,
        url: Url,
        mut recv: mpsc::UnboundedReceiver<WsMessage>,
        send: mpsc::Sender<WsMessage>,
    ) -> MemWsServerFuture {
        let cursor = parse_cursor(&url);
        let frames = self.frames.clone();
        let send_timeout = self.send_timeout;
        let frame_pace = self.frame_pace;
        let consumer_too_slow = self.consumer_too_slow.clone();
        let clock = self.clock.clone();
        self.session_count.fetch_add(1, Ordering::Relaxed);
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
            let clock_for_emit = clock.clone();
            let frame_emit = async move {
                for (id, text) in frames.iter() {
                    if *id < cursor {
                        continue;
                    }
                    if !frame_pace.is_zero() {
                        clock_for_emit.sleep(frame_pace).await;
                    }
                    let res = tokio::time::timeout(
                        send_timeout,
                        send.send(WsMessage::Text(text.clone())),
                    )
                    .await;
                    match res {
                        Ok(Ok(())) => continue,
                        Ok(Err(_)) => return,
                        Err(_) => {
                            consumer_too_slow.fetch_add(1, Ordering::Relaxed);
                            let err_frame = serde_json::json!({
                                "type": "error",
                                "error": "ConsumerTooSlow",
                                "message": format!(
                                    "sim hydrant send_timeout {send_timeout:?} exhausted",
                                ),
                            })
                            .to_string();
                            let _ = tokio::time::timeout(
                                Duration::from_secs(1),
                                send.send(WsMessage::Text(err_frame)),
                            )
                            .await;
                            let _ = tokio::time::timeout(
                                Duration::from_secs(1),
                                send.send(WsMessage::Close {
                                    code: 1011,
                                    reason: "ConsumerTooSlow".into(),
                                }),
                            )
                            .await;
                            return;
                        }
                    }
                }
            };
            let _ = tokio::join!(pong_loop, frame_emit);
        })
    }
}

struct FlapSlingshot {
    cfg: SlingshotFlapConfig,
    clock: Arc<dyn Clock>,
    started_unix: UnixMicros,
}

impl MemHttpResponder for FlapSlingshot {
    fn respond(&self, request: &HttpRequest) -> MemHttpResponse {
        let now = self.clock.now_unix_micros().raw();
        let elapsed_ms = now.saturating_sub(self.started_unix.raw()) / 1000;
        let in_brownout = self.cfg.brownout_enabled
            && elapsed_ms >= self.cfg.brownout_start_ms
            && elapsed_ms < self.cfg.brownout_start_ms + self.cfg.brownout_duration_ms;

        if in_brownout {
            return MemHttpResponse {
                latency: Duration::from_millis(self.cfg.brownout_latency_ms),
                result: Err(NetworkError::Transport("slingshot brownout".into())),
            };
        }

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
                    latency: Duration::from_millis(self.cfg.normal_latency_ms),
                    result: Ok(MemHttpBody::status_only(StatusCode::NOT_FOUND)),
                };
            }
        };

        MemHttpResponse {
            latency: Duration::from_millis(self.cfg.normal_latency_ms),
            result: Ok(MemHttpBody::ok_json(Bytes::from(body))),
        }
    }
}
