use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query, State};
use axum::response::{IntoResponse, Response};
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use http::HeaderMap;
use serde::Deserialize;

use knot_events::{
    BatchEnd, EventCursor, EventLog, ReplayBounds, ReplayBytes, ReplayEvents, Replayed,
};
use knot_runtime::{Clock, HttpTransport};

use crate::XrpcState;
use crate::error::XrpcError;

pub(crate) const EVENTS_ROUTE: &str = "/events";

const DRAIN_BATCH: usize = 100;
const DRAIN_BYTES: usize = 4 << 20;
const MAX_BATCHES_PER_DRAIN: usize = 1_000;
const KEEPALIVE: Duration = Duration::from_secs(30);
const WRITE_DEADLINE: Duration = Duration::from_secs(10);
const TRY_AGAIN_LATER: u16 = 1013;

#[derive(Deserialize)]
pub(crate) struct EventsQuery {
    cursor: Option<String>,
}

pub(crate) async fn events<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    ConnectInfo(socket_peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<EventsQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let peer = state.proxy_trust.client_peer_of(&headers, socket_peer.ip());
    let Some(permit) = state.subscriber_gate.try_admit(peer) else {
        return XrpcError::overloaded(
            "knot is serving its maximum number of event subscribers, retry shortly",
        )
        .into_response();
    };
    let cursor = query
        .cursor
        .as_deref()
        .and_then(|raw| raw.parse::<i64>().ok())
        .map(EventCursor::new)
        .unwrap_or(EventCursor::START);
    let log = Arc::clone(&state.events);
    upgrade.on_upgrade(move |socket| async move {
        let _permit = permit;
        stream_events(socket, log, cursor).await;
    })
}

enum Drained {
    CaughtUp,
    Limited,
}

async fn stream_events<C: Clock>(socket: WebSocket, log: Arc<EventLog<C>>, start: EventCursor) {
    let mut head = log.subscribe();
    let (mut sink, mut from_client) = socket.split();
    let mut keepalive =
        tokio::time::interval_at(tokio::time::Instant::now() + KEEPALIVE, KEEPALIVE);
    let mut cursor = start;
    loop {
        match drain(&mut sink, &log, &mut cursor).await {
            Ok(Drained::CaughtUp) => {}
            Ok(Drained::Limited) => {
                let close = Message::Close(Some(CloseFrame {
                    code: TRY_AGAIN_LATER,
                    reason: "drain limit reached, reconnect to continue".into(),
                }));
                let _ = tokio::time::timeout(WRITE_DEADLINE, sink.send(close)).await;
                return;
            }
            Err(()) => return,
        }
        tokio::select! {
            changed = head.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            _ = keepalive.tick() => {
                let ping = sink.send(Message::Ping(Vec::new().into()));
                if !matches!(tokio::time::timeout(WRITE_DEADLINE, ping).await, Ok(Ok(()))) {
                    return;
                }
            }
            received = from_client.next() => {
                match received {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

fn drain_bounds() -> ReplayBounds {
    ReplayBounds::new(
        ReplayEvents::new(DRAIN_BATCH).expect("drain event maximum is nonzero"),
        ReplayBytes::new(DRAIN_BYTES).expect("drain byte maximum is nonzero"),
    )
}

// who up draining they clock
async fn drain<C: Clock>(
    sink: &mut SplitSink<WebSocket, Message>,
    log: &EventLog<C>,
    cursor: &mut EventCursor,
) -> Result<Drained, ()> {
    let mut batches = 0;
    loop {
        let Replayed { events, end } = log.replay(*cursor, drain_bounds());
        if let Some(last) = events.last() {
            *cursor = last.created;
        }
        let messages: Vec<Result<Message, axum::Error>> = events
            .iter()
            .map(|event| {
                Ok(Message::Text(
                    serde_json::to_string(event.as_ref())
                        .expect("wire event serializes to JSON")
                        .into(),
                ))
            })
            .collect();
        drop(events);
        let sent = tokio::time::timeout(
            WRITE_DEADLINE,
            sink.send_all(&mut futures::stream::iter(messages)),
        )
        .await;
        if !matches!(sent, Ok(Ok(()))) {
            return Err(());
        }
        if end == BatchEnd::CaughtUp {
            return Ok(Drained::CaughtUp);
        }
        batches += 1;
        if batches == MAX_BATCHES_PER_DRAIN {
            return Ok(Drained::Limited);
        }
    }
}
