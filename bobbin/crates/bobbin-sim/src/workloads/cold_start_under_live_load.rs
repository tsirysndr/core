use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bobbin_runtime::{Clock, MemHttpResponder, MemWsResponder, MemWsServerFuture, WsMessage};
use tokio::sync::mpsc;
use url::Url;

use crate::report::{SimOutcome, SimReport};
use crate::workload::{Workload, WorkloadCtx, WorkloadHooks};
use crate::workloads::util::{AssertNoSlingshot, format_rkey, format_tid};

const NAME: &str = "cold-start-under-live-load";

#[derive(Clone, Debug)]
pub struct ColdStartUnderLiveLoadConfig {
    pub replay_frames: usize,
    pub live_frames: usize,
    pub live_pace_us: u64,
    pub replay_pace_us: u64,
}

impl Default for ColdStartUnderLiveLoadConfig {
    fn default() -> Self {
        Self {
            replay_frames: 200,
            live_frames: 50,
            live_pace_us: 1_000,
            replay_pace_us: 0,
        }
    }
}

pub struct ColdStartUnderLiveLoad {
    config: ColdStartUnderLiveLoadConfig,
}

impl ColdStartUnderLiveLoad {
    pub fn new(config: ColdStartUnderLiveLoadConfig) -> Self {
        Self { config }
    }
}

impl Workload for ColdStartUnderLiveLoad {
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

        let total_frames = (cfg.replay_frames + cfg.live_frames) as u64;
        let frame_log = build_frame_log(cfg.replay_frames, cfg.live_frames);

        let hydrant: Arc<dyn MemWsResponder> = Arc::new(LiveLoadHydrant {
            frames: Mutex::new(Some(frame_log)),
            replay_pace: Duration::from_micros(cfg.replay_pace_us),
            live_pace: Duration::from_micros(cfg.live_pace_us),
            replay_count: cfg.replay_frames,
            clock: clock.clone(),
        });
        let slingshot_probe = AssertNoSlingshot::new();
        let slingshot: Arc<dyn MemHttpResponder> = Arc::new(slingshot_probe.clone());

        let script = Box::pin(async move {
            let started = clock.now_unix_micros();
            let mut rx = coverage.subscribe();
            let outcome = loop {
                let snap = *rx.borrow_and_update();
                if snap.events_processed() >= total_frames {
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
            let snap = coverage.snapshot();
            let virtual_runtime =
                Duration::from_micros(clock.now_unix_micros().raw().saturating_sub(started.raw()));
            let stray_slingshot = slingshot_probe.calls();
            let outcome = match outcome {
                SimOutcome::Passed if stray_slingshot != 0 => SimOutcome::Failed,
                other => other,
            };
            let failure_reason = match outcome {
                SimOutcome::Passed => None,
                SimOutcome::Failed => Some(format!(
                    "events={} target={} last_cursor={} stray_slingshot={}",
                    snap.events_processed(),
                    total_frames,
                    snap.last_cursor().raw(),
                    stray_slingshot,
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

fn build_frame_log(replay: usize, live: usize) -> Vec<(bool, String)> {
    let total = replay + live;
    (0..total)
        .map(|i| {
            let id = (i + 1) as u64;
            let is_live = i >= replay;
            let owner = format!("did:plc:owner-{i}");
            let body = serde_json::json!({
                "id": id,
                "type": "record",
                "record": {
                    "live": is_live,
                    "did": owner,
                    "rev": format_tid(i),
                    "collection": "sh.tangled.repo",
                    "rkey": format_rkey(i),
                    "action": "create",
                    "record": {
                        "$type": "sh.tangled.repo",
                        "createdAt": "2026-05-01T00:00:00Z",
                        "knot": "oyster.cafe",
                        "name": format!("repo-{i}"),
                        "repoDid": format!("did:plc:repo-{i}")
                    }
                }
            })
            .to_string();
            (is_live, body)
        })
        .collect()
}

struct LiveLoadHydrant {
    frames: Mutex<Option<Vec<(bool, String)>>>,
    replay_pace: Duration,
    live_pace: Duration,
    replay_count: usize,
    clock: Arc<dyn Clock>,
}

impl MemWsResponder for LiveLoadHydrant {
    fn spawn_server(
        &self,
        _: Url,
        mut recv: mpsc::UnboundedReceiver<WsMessage>,
        send: mpsc::Sender<WsMessage>,
    ) -> MemWsServerFuture {
        let frames = self.frames.lock().unwrap().take().unwrap_or_default();
        let replay_pace = self.replay_pace;
        let live_pace = self.live_pace;
        let replay_count = self.replay_count;
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
                for (i, (is_live, text)) in frames.into_iter().enumerate() {
                    let pace = if is_live { live_pace } else { replay_pace };
                    if !pace.is_zero() {
                        clock.sleep(pace).await;
                    }
                    if send.send(WsMessage::Text(text)).await.is_err() {
                        return;
                    }
                    if i + 1 == replay_count {
                        clock.sleep(Duration::from_millis(1)).await;
                    }
                }
            };
            let _ = tokio::join!(pong_loop, frame_emit);
        })
    }
}
