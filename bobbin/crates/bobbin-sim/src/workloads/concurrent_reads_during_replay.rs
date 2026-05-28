use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bobbin_edge_index::{PageCursor, PageLimit, SortDir};
use bobbin_runtime::{Clock, MemHttpResponder, MemWsResponder, MemWsServerFuture, WsMessage};
use bobbin_types::ids::{EdgeKey, SubjectRef};
use jacquard_common::DefaultStr;
use jacquard_common::types::did::Did;
use jacquard_common::types::nsid::Nsid;
use tokio::sync::mpsc;
use url::Url;

use crate::report::{SimOutcome, SimReport};
use crate::workload::{Workload, WorkloadCtx, WorkloadHooks};
use crate::workloads::util::{AssertNoSlingshot, format_rkey, format_tid};

const NAME: &str = "concurrent-reads-during-replay";

fn target_did() -> Did<DefaultStr> {
    Did::new_static("did:plc:lyna").expect("literal is a valid DID")
}

#[derive(Clone, Debug)]
pub struct ConcurrentReadsDuringReplayConfig {
    pub follow_frames: usize,
    pub frame_pace_us: u64,
    pub read_pace_us: u64,
}

impl Default for ConcurrentReadsDuringReplayConfig {
    fn default() -> Self {
        Self {
            follow_frames: 200,
            frame_pace_us: 100,
            read_pace_us: 50,
        }
    }
}

pub struct ConcurrentReadsDuringReplay {
    config: ConcurrentReadsDuringReplayConfig,
}

impl ConcurrentReadsDuringReplay {
    pub fn new(config: ConcurrentReadsDuringReplayConfig) -> Self {
        Self { config }
    }
}

impl Workload for ConcurrentReadsDuringReplay {
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

        let total = cfg.follow_frames as u64;
        let frame_log = build_frame_log(cfg.follow_frames);

        let hydrant: Arc<dyn MemWsResponder> = Arc::new(FollowHydrant {
            frames: Mutex::new(Some(frame_log)),
            frame_pace: Duration::from_micros(cfg.frame_pace_us),
            clock: clock.clone(),
        });
        let slingshot_probe = AssertNoSlingshot::new();
        let slingshot: Arc<dyn MemHttpResponder> = Arc::new(slingshot_probe.clone());

        let read_clock = clock.clone();
        let read_store = store.clone();
        let read_cancel = cancel.clone();
        let read_pace = Duration::from_micros(cfg.read_pace_us);
        let monotonicity_violations = Arc::new(AtomicU64::new(0));
        let max_observed = Arc::new(AtomicU64::new(0));
        let read_iterations = Arc::new(AtomicU64::new(0));
        let monotonicity_violations_w = monotonicity_violations.clone();
        let max_observed_w = max_observed.clone();
        let read_iterations_w = read_iterations.clone();

        let read_key = EdgeKey::new(
            Nsid::new_static("sh.tangled.graph.follow").unwrap(),
            SubjectRef::Did(target_did()),
        );
        let final_key = read_key.clone();

        let reader_task = tokio::spawn(async move {
            let mut last_count: u64 = 0;
            let mut last_list_len: usize = 0;
            loop {
                tokio::select! {
                    biased;
                    _ = read_cancel.cancelled() => return,
                    _ = read_clock.sleep(read_pace) => {}
                }
                let count = read_store.count(&read_key);
                let listed = read_store.list(
                    &read_key,
                    PageCursor::Start,
                    PageLimit::new(50).unwrap(),
                    SortDir::Asc,
                );
                if count < last_count || listed.items.len() < last_list_len {
                    monotonicity_violations_w.fetch_add(1, Ordering::Relaxed);
                }
                last_count = count.max(last_count);
                last_list_len = listed.items.len().max(last_list_len);
                max_observed_w.store(last_count, Ordering::Relaxed);
                read_iterations_w.fetch_add(1, Ordering::Relaxed);
            }
        });

        let script = Box::pin(async move {
            let started = clock.now_unix_micros();
            let mut rx = coverage.subscribe();
            let outcome = loop {
                let snap = *rx.borrow_and_update();
                if snap.events_processed() >= total {
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
            let _ = reader_task.await;

            let snap = coverage.snapshot();
            let virtual_runtime =
                Duration::from_micros(clock.now_unix_micros().raw().saturating_sub(started.raw()));
            let violations = monotonicity_violations.load(Ordering::Relaxed);
            let observed = max_observed.load(Ordering::Relaxed);
            let iters = read_iterations.load(Ordering::Relaxed);
            let final_count = store.count(&final_key);
            let stray_slingshot = slingshot_probe.calls();
            let outcome = match outcome {
                SimOutcome::Passed
                    if violations == 0 && final_count == total && stray_slingshot == 0 =>
                {
                    SimOutcome::Passed
                }
                SimOutcome::Passed => SimOutcome::Failed,
                other => other,
            };
            let failure_reason = match outcome {
                SimOutcome::Passed => None,
                SimOutcome::Failed => Some(format!(
                    "events={} target={} reads={} max_observed_count={} final_count={} \
                     monotonicity_violations={} stray_slingshot={}",
                    snap.events_processed(),
                    total,
                    iters,
                    observed,
                    final_count,
                    violations,
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

fn build_frame_log(count: usize) -> Vec<String> {
    let target = target_did();
    (0..count)
        .map(|i| {
            let id = (i + 1) as u64;
            let actor =
                Did::<DefaultStr>::new_owned(format!("did:plc:nautilus-{i}")).expect("valid DID");
            serde_json::json!({
                "id": id,
                "type": "record",
                "record": {
                    "live": false,
                    "did": actor.as_ref(),
                    "rev": format_tid(i),
                    "collection": "sh.tangled.graph.follow",
                    "rkey": format_rkey(i),
                    "action": "create",
                    "record": {
                        "$type": "sh.tangled.graph.follow",
                        "createdAt": "2026-05-01T00:00:00Z",
                        "subject": target.as_ref(),
                    }
                }
            })
            .to_string()
        })
        .collect()
}

struct FollowHydrant {
    frames: Mutex<Option<Vec<String>>>,
    frame_pace: Duration,
    clock: Arc<dyn Clock>,
}

impl MemWsResponder for FollowHydrant {
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
