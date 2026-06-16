use std::sync::{Arc, Mutex};
use std::time::Duration;

use bobbin_knot_proxy::KnotHost;
use bobbin_runtime::{Clock, WsConn, WsMessage, WsTransport};
use jacquard_common::DefaultStr;
use jacquard_common::types::did::Did;
use serde::Deserialize;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::client::authority;
use crate::roster::{AclOp, Cursor, Roster};

const KNOT_MEMBER_UPDATE_NSID: &str = "sh.tangled.knot.memberUpdate";
const REPO_COLLABORATOR_UPDATE_NSID: &str = "sh.tangled.repo.collaboratorUpdate";
const RECONNECT_INITIAL: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(60);
const HEALTHY_SESSION_MIN: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct StreamConfig {
    pub ws: Arc<dyn WsTransport>,
    pub clock: Arc<dyn Clock>,
    pub cancel: CancellationToken,
}

enum SessionEnd {
    Cancelled,
    Closed { progressed: bool },
    ConnectFailed,
}

pub async fn run_stream(
    cfg: &StreamConfig,
    host: &KnotHost,
    roster: &Mutex<Roster>,
    initial_cursor: i64,
) {
    let mut cursor = initial_cursor;
    let mut backoff = RECONNECT_INITIAL;
    loop {
        if cfg.cancel.is_cancelled() {
            return;
        }
        let started = cfg.clock.now_instant();
        let end = run_session(cfg, host, roster, &mut cursor).await;
        if matches!(&end, SessionEnd::Cancelled) {
            return;
        }
        let elapsed = cfg.clock.now_instant().saturating_duration_since(started);
        if matches!(&end, SessionEnd::Closed { progressed: false }) && cursor != 0 {
            cursor = 0;
        }
        if session_was_healthy(&end, elapsed) {
            backoff = RECONNECT_INITIAL;
        }
        let delay = jitter_delay(backoff, cfg.clock.now_unix_micros().raw());
        tokio::select! {
            _ = cfg.cancel.cancelled() => return,
            _ = cfg.clock.sleep(delay) => {}
        }
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

fn session_was_healthy(end: &SessionEnd, elapsed: Duration) -> bool {
    matches!(end, SessionEnd::Closed { progressed: true }) && elapsed >= HEALTHY_SESSION_MIN
}

fn jitter_delay(base: Duration, entropy: u64) -> Duration {
    let frac = (entropy % 1024) as f64 / 1024.0;
    base.mul_f64(0.5 + 0.5 * frac)
}

async fn run_session(
    cfg: &StreamConfig,
    host: &KnotHost,
    roster: &Mutex<Roster>,
    cursor: &mut i64,
) -> SessionEnd {
    let Some(url) = events_url(host, *cursor) else {
        return SessionEnd::ConnectFailed;
    };
    let conn = tokio::select! {
        _ = cfg.cancel.cancelled() => return SessionEnd::Cancelled,
        res = cfg.ws.connect(url) => match res {
            Ok(conn) => conn,
            Err(err) => {
                tracing::warn!(host = %authority(host), error = %err, "knot eventstream connect failed");
                return SessionEnd::ConnectFailed;
            }
        },
    };
    let WsConn {
        mut sink,
        mut stream,
    } = conn;
    let mut progressed = false;
    loop {
        let message = tokio::select! {
            _ = cfg.cancel.cancelled() => return SessionEnd::Cancelled,
            message = stream.next() => message,
        };
        match message {
            None => return SessionEnd::Closed { progressed },
            Some(Ok(WsMessage::Text(text))) => {
                progressed = true;
                process_frame(&text, roster, cursor);
            }
            Some(Ok(WsMessage::Ping(payload))) => {
                progressed = true;
                let _ = sink.send(WsMessage::Pong(payload)).await;
            }
            Some(Ok(WsMessage::Close { .. })) => return SessionEnd::Closed { progressed },
            Some(Ok(_)) => {}
            Some(Err(err)) => {
                tracing::warn!(host = %authority(host), error = %err, "knot eventstream read error");
                return SessionEnd::Closed { progressed };
            }
        }
    }
}

fn process_frame(text: &str, roster: &Mutex<Roster>, cursor: &mut i64) {
    let Ok(frame) = serde_json::from_str::<FrameWire>(text) else {
        return;
    };
    *cursor = frame.created;
    match frame.nsid.as_str() {
        KNOT_MEMBER_UPDATE_NSID => {
            if let Ok(update) = serde_json::from_str::<MemberUpdate>(frame.event.get()) {
                roster.lock().unwrap().apply_member(
                    update.op,
                    update.subject,
                    Cursor(frame.created),
                );
            }
        }
        REPO_COLLABORATOR_UPDATE_NSID => {
            if let Ok(update) = serde_json::from_str::<CollaboratorUpdate>(frame.event.get()) {
                roster.lock().unwrap().apply_collaborator(
                    update.op,
                    update.repo,
                    update.subject,
                    Cursor(frame.created),
                );
            }
        }
        _ => {}
    }
}

fn events_url(host: &KnotHost, cursor: i64) -> Option<Url> {
    let scheme = if host.url().scheme() == "https" {
        "wss"
    } else {
        "ws"
    };
    let base = format!("{scheme}://{}/events", authority(host));
    let full = if cursor != 0 {
        format!("{base}?cursor={cursor}")
    } else {
        base
    };
    Url::parse(&full).ok()
}

#[derive(Deserialize)]
struct FrameWire {
    nsid: String,
    event: Box<RawValue>,
    created: i64,
}

#[derive(Deserialize)]
struct MemberUpdate {
    op: AclOp,
    subject: Did<DefaultStr>,
}

#[derive(Deserialize)]
struct CollaboratorUpdate {
    op: AclOp,
    subject: Did<DefaultStr>,
    repo: Did<DefaultStr>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use bobbin_edge_index::EdgeStore;
    use bobbin_runtime::{
        NetworkError, SystemClock, WsConnectFuture, WsMessageFuture, WsSendFuture, WsSink, WsStream,
    };
    use bobbin_types::ids::{EdgeKey, SubjectRef, nsid_static};
    use bobbin_types::knot_acl;
    use bytes::Bytes;

    use crate::registry::KnotRegistry;

    struct ScriptStream {
        msgs: VecDeque<WsMessage>,
    }
    impl WsStream for ScriptStream {
        fn next<'a>(&'a mut self) -> WsMessageFuture<'a> {
            Box::pin(async move { self.msgs.pop_front().map(Ok) })
        }
    }

    struct RecordSink {
        sent: Arc<Mutex<Vec<WsMessage>>>,
    }
    impl WsSink for RecordSink {
        fn send<'a>(&'a mut self, message: WsMessage) -> WsSendFuture<'a> {
            let sent = self.sent.clone();
            Box::pin(async move {
                sent.lock().unwrap().push(message);
                Ok(())
            })
        }
    }

    struct ScriptWs {
        msgs: Mutex<Option<VecDeque<WsMessage>>>,
        sent: Arc<Mutex<Vec<WsMessage>>>,
        fail: bool,
    }
    impl WsTransport for ScriptWs {
        fn connect(&self, _url: Url) -> WsConnectFuture {
            if self.fail {
                return Box::pin(async {
                    Err(NetworkError::Connect("scripted failure".to_owned()))
                });
            }
            let msgs = self.msgs.lock().unwrap().take().unwrap_or_default();
            let sent = self.sent.clone();
            Box::pin(async move {
                Ok(WsConn {
                    sink: Box::new(RecordSink { sent }),
                    stream: Box::new(ScriptStream { msgs }),
                })
            })
        }
    }

    fn member_frame(op: &str, subject: &str, created: i64) -> String {
        format!(
            r#"{{"rkey":"r{created}","nsid":"sh.tangled.knot.memberUpdate","event":{{"op":"{op}","subject":"{subject}"}},"created":{created}}}"#
        )
    }

    fn collab_frame(op: &str, subject: &str, repo: &str, created: i64) -> String {
        format!(
            r#"{{"rkey":"r{created}","nsid":"sh.tangled.repo.collaboratorUpdate","event":{{"op":"{op}","subject":"{subject}","repo":"{repo}"}},"created":{created}}}"#
        )
    }

    fn did(s: &str) -> Did<DefaultStr> {
        Did::new_owned(s).unwrap()
    }

    fn member_count(store: &EdgeStore, subject: &str) -> u64 {
        store.count(&EdgeKey::new(
            nsid_static("sh.tangled.knot.member"),
            SubjectRef::Did(did(subject)),
        ))
    }

    fn collaborator_count(store: &EdgeStore, repo: &str) -> u64 {
        store.count(&EdgeKey::new(
            nsid_static("sh.tangled.repo.collaborator"),
            SubjectRef::Did(did(repo)),
        ))
    }

    fn cfg(ws: Arc<dyn WsTransport>, cancel: CancellationToken) -> StreamConfig {
        StreamConfig {
            ws,
            clock: Arc::new(SystemClock::new()),
            cancel,
        }
    }

    #[tokio::test]
    async fn session_dispatches_deltas_pongs_and_advances_cursor() {
        let store = Arc::new(EdgeStore::new(bobbin_runtime::RuntimeHasher::default()));
        let knot = knot_acl::host_to_knot_did("oyster.cafe").unwrap();
        let registry = Arc::new(KnotRegistry::new());
        registry.observe_repo(
            &knot_acl::KnotHostKey::new("oyster.cafe"),
            Did::new_owned("did:plc:scallop").unwrap(),
        );
        let roster = Mutex::new(Roster::new(
            store.clone(),
            knot,
            registry,
            knot_acl::KnotHostKey::new("oyster.cafe"),
        ));
        let frames = VecDeque::from(vec![
            WsMessage::Text(member_frame("add", "did:plc:boltless", 100)),
            WsMessage::Text(collab_frame(
                "add",
                "did:plc:olaren",
                "did:plc:scallop",
                200,
            )),
            WsMessage::Ping(Bytes::from_static(b"ka")),
            WsMessage::Text(member_frame("remove", "did:plc:boltless", 300)),
            WsMessage::Close {
                code: 1000,
                reason: String::new(),
            },
        ]);
        let sent = Arc::new(Mutex::new(Vec::new()));
        let ws: Arc<dyn WsTransport> = Arc::new(ScriptWs {
            msgs: Mutex::new(Some(frames)),
            sent: sent.clone(),
            fail: false,
        });
        let config = cfg(ws, CancellationToken::new());
        let host = KnotHost::parse("http://oyster.cafe").unwrap();
        let mut cursor = 0i64;

        let end = run_session(&config, &host, &roster, &mut cursor).await;

        assert!(matches!(end, SessionEnd::Closed { progressed: true }));
        assert_eq!(cursor, 300);
        assert_eq!(member_count(&store, "did:plc:boltless"), 0);
        assert_eq!(collaborator_count(&store, "did:plc:scallop"), 1);
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(matches!(&sent[0], WsMessage::Pong(p) if p.as_ref() == b"ka"));
    }

    #[tokio::test]
    async fn session_reports_no_progress_on_immediate_close() {
        let store = Arc::new(EdgeStore::new(bobbin_runtime::RuntimeHasher::default()));
        let knot = knot_acl::host_to_knot_did("oyster.cafe").unwrap();
        let roster = Mutex::new(Roster::new(
            store,
            knot,
            Arc::new(KnotRegistry::new()),
            knot_acl::KnotHostKey::new("oyster.cafe"),
        ));
        let frames = VecDeque::from(vec![WsMessage::Close {
            code: 1000,
            reason: String::new(),
        }]);
        let ws: Arc<dyn WsTransport> = Arc::new(ScriptWs {
            msgs: Mutex::new(Some(frames)),
            sent: Arc::new(Mutex::new(Vec::new())),
            fail: false,
        });
        let config = cfg(ws, CancellationToken::new());
        let host = KnotHost::parse("http://oyster.cafe").unwrap();
        let mut cursor = 99i64;

        let end = run_session(&config, &host, &roster, &mut cursor).await;

        assert!(
            matches!(end, SessionEnd::Closed { progressed: false }),
            "a session that delivers no frames before closing reports no progress"
        );
    }

    #[tokio::test]
    async fn pre_cancelled_stream_returns_without_connecting() {
        let store = Arc::new(EdgeStore::new(bobbin_runtime::RuntimeHasher::default()));
        let knot = knot_acl::host_to_knot_did("oyster.cafe").unwrap();
        let roster = Mutex::new(Roster::new(
            store,
            knot,
            Arc::new(KnotRegistry::new()),
            knot_acl::KnotHostKey::new("oyster.cafe"),
        ));
        let ws: Arc<dyn WsTransport> = Arc::new(ScriptWs {
            msgs: Mutex::new(None),
            sent: Arc::new(Mutex::new(Vec::new())),
            fail: true,
        });
        let cancel = CancellationToken::new();
        cancel.cancel();
        let config = cfg(ws, cancel);
        let host = KnotHost::parse("http://oyster.cafe").unwrap();

        run_stream(&config, &host, &roster, 0).await;
    }

    #[test]
    fn events_url_carries_scheme_and_cursor() {
        let host = KnotHost::parse("http://oyster.cafe").unwrap();
        assert_eq!(
            events_url(&host, 0).unwrap().as_str(),
            "ws://oyster.cafe/events"
        );
        assert_eq!(
            events_url(&host, 42).unwrap().as_str(),
            "ws://oyster.cafe/events?cursor=42"
        );
        let secure = KnotHost::parse("https://nel.pet").unwrap();
        assert_eq!(
            events_url(&secure, 7).unwrap().as_str(),
            "wss://nel.pet/events?cursor=7"
        );
    }

    #[test]
    fn only_a_long_lived_session_resets_backoff() {
        assert!(session_was_healthy(
            &SessionEnd::Closed { progressed: true },
            HEALTHY_SESSION_MIN
        ));
        assert!(
            !session_was_healthy(
                &SessionEnd::Closed { progressed: true },
                HEALTHY_SESSION_MIN - Duration::from_millis(1)
            ),
            "a knot that flaps faster than the healthy floor must keep backing off"
        );
        assert!(!session_was_healthy(
            &SessionEnd::Closed { progressed: false },
            Duration::from_secs(3600)
        ));
        assert!(!session_was_healthy(
            &SessionEnd::ConnectFailed,
            Duration::from_secs(3600)
        ));
    }

    #[test]
    fn jitter_delay_stays_within_half_to_full_window() {
        let base = Duration::from_secs(8);
        [0u64, 1, 511, 512, 1023, 1024, u64::MAX]
            .into_iter()
            .for_each(|entropy| {
                let delay = jitter_delay(base, entropy);
                assert!(delay >= base / 2, "delay {delay:?} below half of {base:?}");
                assert!(delay <= base, "delay {delay:?} above {base:?}");
            });
    }
}
