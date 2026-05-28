use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bobbin_runtime::{MemHttpResponder, MemWsResponder, MemWsServerFuture, WsMessage};
use jacquard_common::DefaultStr;
use jacquard_common::types::did::Did;
use tokio::sync::mpsc;
use url::Url;

use crate::report::{SimOutcome, SimReport};
use crate::workload::{Workload, WorkloadCtx, WorkloadHooks};
use crate::workloads::util::{AssertNoSlingshot, format_rkey, format_tid};

const NAME: &str = "hydrant-disconnect-barrage";

fn authority_did() -> Did<DefaultStr> {
    Did::new_static("did:plc:squid").expect("literal is a valid DID")
}

#[derive(Clone, Debug)]
pub struct HydrantDisconnectBarrageConfig {
    pub frames: usize,
    pub disconnect_after_frames_per_session: Vec<usize>,
}

impl Default for HydrantDisconnectBarrageConfig {
    fn default() -> Self {
        Self {
            frames: 256,
            disconnect_after_frames_per_session: vec![50, 80, 60],
        }
    }
}

pub struct HydrantDisconnectBarrage {
    config: HydrantDisconnectBarrageConfig,
}

impl HydrantDisconnectBarrage {
    pub fn new(config: HydrantDisconnectBarrageConfig) -> Self {
        Self { config }
    }
}

impl Workload for HydrantDisconnectBarrage {
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

        let frames_total = cfg.frames as u64;
        let frame_log = build_frame_log(cfg.frames);
        let drop_schedule = Arc::new(Mutex::new(cfg.disconnect_after_frames_per_session.clone()));
        let session_count = Arc::new(AtomicU64::new(0));
        let disconnect_count = Arc::new(AtomicU64::new(0));

        let hydrant: Arc<dyn MemWsResponder> = Arc::new(BarrageHydrant {
            frame_log: Arc::new(frame_log),
            drop_schedule,
            session_count: session_count.clone(),
            disconnect_count: disconnect_count.clone(),
        });
        let slingshot_probe = AssertNoSlingshot::new();
        let slingshot: Arc<dyn MemHttpResponder> = Arc::new(slingshot_probe.clone());

        let disconnects = disconnect_count.clone();
        let sessions = session_count.clone();
        let script = Box::pin(async move {
            let started = clock.now_unix_micros();
            let mut rx = coverage.subscribe();
            let outcome = loop {
                let snap = *rx.borrow_and_update();
                if snap.events_processed() == frames_total {
                    break SimOutcome::Passed;
                }
                if snap.events_processed() > frames_total {
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
            let dc = disconnects.load(Ordering::Relaxed);
            let ss = sessions.load(Ordering::Relaxed);
            let stray_slingshot = slingshot_probe.calls();
            let outcome = match outcome {
                SimOutcome::Passed if dc == 0 => SimOutcome::Failed,
                SimOutcome::Passed if stray_slingshot != 0 => SimOutcome::Failed,
                other => other,
            };
            let failure_reason = match outcome {
                SimOutcome::Passed => None,
                SimOutcome::Failed => Some(format!(
                    "events={} target={} disconnects={} sessions={} stray_slingshot={}",
                    snap.events_processed(),
                    frames_total,
                    dc,
                    ss,
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

fn build_frame_log(count: usize) -> Vec<(u64, String)> {
    let authority = authority_did();
    (0..count)
        .map(|i| {
            let id = (i + 1) as u64;
            let repo_did =
                Did::<DefaultStr>::new_owned(format!("did:plc:squid-{i}")).expect("valid DID");
            let body = serde_json::json!({
                "id": id,
                "type": "record",
                "record": {
                    "live": false,
                    "did": authority.as_ref(),
                    "rev": format_tid(i),
                    "collection": "sh.tangled.repo",
                    "rkey": format_rkey(i),
                    "action": "create",
                    "record": {
                        "$type": "sh.tangled.repo",
                        "createdAt": "2026-05-01T00:00:00Z",
                        "knot": "oyster.cafe",
                        "name": format!("repo-{i}"),
                        "repoDid": repo_did.as_ref()
                    }
                }
            })
            .to_string();
            (id, body)
        })
        .collect()
}

struct BarrageHydrant {
    frame_log: Arc<Vec<(u64, String)>>,
    drop_schedule: Arc<Mutex<Vec<usize>>>,
    session_count: Arc<AtomicU64>,
    disconnect_count: Arc<AtomicU64>,
}

impl MemWsResponder for BarrageHydrant {
    fn spawn_server(
        &self,
        url: Url,
        mut recv: mpsc::UnboundedReceiver<WsMessage>,
        send: mpsc::Sender<WsMessage>,
    ) -> MemWsServerFuture {
        let cursor = parse_cursor(&url).unwrap_or(0);
        let frame_log = self.frame_log.clone();
        let drop_after = {
            let mut guard = self.drop_schedule.lock().unwrap();
            if guard.is_empty() {
                usize::MAX
            } else {
                guard.remove(0)
            }
        };
        self.session_count.fetch_add(1, Ordering::Relaxed);
        let disconnects = self.disconnect_count.clone();
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
                let mut emitted = 0usize;
                for (id, body) in frame_log.iter() {
                    if *id < cursor {
                        continue;
                    }
                    if send.send(WsMessage::Text(body.clone())).await.is_err() {
                        return;
                    }
                    emitted += 1;
                    if emitted >= drop_after {
                        disconnects.fetch_add(1, Ordering::Relaxed);
                        let _ = send
                            .send(WsMessage::Close {
                                code: 1011,
                                reason: "scripted disconnect".into(),
                            })
                            .await;
                        return;
                    }
                }
            };
            let _ = tokio::join!(pong_loop, frame_emit);
        })
    }
}

fn parse_cursor(url: &Url) -> Option<u64> {
    for (k, v) in url.query_pairs() {
        if k == "cursor" {
            return v.parse().ok();
        }
    }
    None
}
