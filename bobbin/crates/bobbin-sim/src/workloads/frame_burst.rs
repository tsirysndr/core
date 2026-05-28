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

const NAME: &str = "frame-burst";

fn authority_did() -> Did<DefaultStr> {
    Did::new_static("did:plc:squid").expect("literal is a valid DID")
}

pub struct FrameBurst {
    frames: usize,
}

impl FrameBurst {
    pub fn new(frames: usize) -> Self {
        Self { frames }
    }
}

impl Workload for FrameBurst {
    fn name(&self) -> &'static str {
        NAME
    }

    fn build(self: Box<Self>, ctx: WorkloadCtx) -> WorkloadHooks {
        let count = self.frames;
        let frames = build_frame_script(count);
        let session_count = Arc::new(AtomicU64::new(0));

        let hydrant: Arc<dyn MemWsResponder> = Arc::new(BurstHydrant {
            frames: Mutex::new(Some(frames)),
            session_count: session_count.clone(),
        });
        let slingshot_probe = AssertNoSlingshot::new();
        let slingshot: Arc<dyn MemHttpResponder> = Arc::new(slingshot_probe.clone());

        let WorkloadCtx {
            seed,
            clock,
            coverage,
            store,
            cancel,
            ..
        } = ctx;
        let target = count as u64;
        let sessions = session_count.clone();

        let script = Box::pin(async move {
            let mut rx = coverage.subscribe();
            let started = clock.now_unix_micros();
            let outcome = loop {
                let snap = *rx.borrow_and_update();
                if snap.events_processed() >= target {
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
            let session_total = sessions.load(Ordering::Relaxed);
            let outcome = match outcome {
                SimOutcome::Passed if stray_slingshot == 0 && session_total == 1 => {
                    SimOutcome::Passed
                }
                SimOutcome::Passed => SimOutcome::Failed,
                other => other,
            };
            let failure_reason = match outcome {
                SimOutcome::Passed => None,
                SimOutcome::Failed => Some(format!(
                    "events={}/{} stray_slingshot={} sessions={}",
                    snap.events_processed(),
                    target,
                    stray_slingshot,
                    session_total,
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

fn build_frame_script(count: usize) -> Vec<String> {
    let authority = authority_did();
    (0..count)
        .map(|i| {
            let repo_did =
                Did::<DefaultStr>::new_owned(format!("did:plc:squid-{i}")).expect("valid DID");
            serde_json::json!({
                "id": (i + 1) as u64,
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
            .to_string()
        })
        .collect()
}

struct BurstHydrant {
    frames: Mutex<Option<Vec<String>>>,
    session_count: Arc<AtomicU64>,
}

impl MemWsResponder for BurstHydrant {
    fn spawn_server(
        &self,
        _: Url,
        mut recv: mpsc::UnboundedReceiver<WsMessage>,
        send: mpsc::Sender<WsMessage>,
    ) -> MemWsServerFuture {
        let frames = self.frames.lock().unwrap().take().unwrap_or_default();
        self.session_count.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            for text in frames {
                if send.send(WsMessage::Text(text)).await.is_err() {
                    return;
                }
            }
            loop {
                match recv.recv().await {
                    Some(WsMessage::Ping(payload)) => {
                        if send.send(WsMessage::Pong(payload)).await.is_err() {
                            return;
                        }
                    }
                    Some(WsMessage::Close { .. }) | None => return,
                    Some(_) => {}
                }
            }
        })
    }
}
