use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::stream;
use http::{HeaderMap, StatusCode};
use tokio::sync::mpsc;
use url::Url;

use crate::clock::{Clock, SleepFuture};
use crate::network::{
    BodyStream, HttpRequest, HttpResponseFuture, HttpResponseHead, HttpResult, HttpTransport,
    NetworkError, WsConn, WsConnectFuture, WsMessage, WsMessageFuture, WsSendFuture, WsSink,
    WsStream, WsTransport,
};

#[derive(Debug)]
pub struct MemHttpResponse {
    pub latency: Duration,
    pub result: Result<MemHttpBody, NetworkError>,
}

#[derive(Debug)]
pub struct MemHttpBody {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl MemHttpBody {
    pub fn ok_json(body: Bytes) -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        Self {
            status: StatusCode::OK,
            headers,
            body,
        }
    }

    pub fn status_only(status: StatusCode) -> Self {
        Self {
            status,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        }
    }
}

pub trait MemHttpResponder: Send + Sync + 'static {
    fn respond(&self, request: &HttpRequest) -> MemHttpResponse;
}

#[derive(Clone)]
pub struct MemHttpTransport {
    responder: Arc<dyn MemHttpResponder>,
    clock: Arc<dyn Clock>,
}

impl MemHttpTransport {
    pub fn new(responder: Arc<dyn MemHttpResponder>, clock: Arc<dyn Clock>) -> Self {
        Self { responder, clock }
    }

    pub fn shared(
        responder: Arc<dyn MemHttpResponder>,
        clock: Arc<dyn Clock>,
    ) -> Arc<dyn HttpTransport> {
        Arc::new(Self::new(responder, clock))
    }
}

impl HttpTransport for MemHttpTransport {
    fn execute(&self, request: HttpRequest) -> HttpResponseFuture {
        let response = self.responder.respond(&request);
        let sleep: SleepFuture = self.clock.sleep(response.latency);
        Box::pin(async move {
            sleep.await;
            into_response_head(response.result)
        })
    }
}

fn into_response_head(result: Result<MemHttpBody, NetworkError>) -> HttpResult {
    let body = result?;
    let content_length = Some(body.body.len() as u64);
    let body_stream: BodyStream = Box::pin(stream::once(async move { Ok(body.body) }));
    Ok(HttpResponseHead {
        status: body.status,
        headers: body.headers,
        content_length,
        body: body_stream,
    })
}

pub type MemWsServerFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

pub trait MemWsResponder: Send + Sync + 'static {
    fn spawn_server(
        &self,
        url: Url,
        recv: mpsc::UnboundedReceiver<WsMessage>,
        send: mpsc::Sender<WsMessage>,
    ) -> MemWsServerFuture;
}

pub const DEFAULT_MEM_WS_CAPACITY: usize = 4096;

#[derive(Clone)]
pub struct MemWsTransport {
    responder: Arc<dyn MemWsResponder>,
    capacity: usize,
}

impl MemWsTransport {
    pub fn new(responder: Arc<dyn MemWsResponder>) -> Self {
        Self::with_capacity(responder, DEFAULT_MEM_WS_CAPACITY)
    }

    pub fn with_capacity(responder: Arc<dyn MemWsResponder>, capacity: usize) -> Self {
        assert!(capacity > 0, "MemWsTransport capacity must be > 0");
        Self {
            responder,
            capacity,
        }
    }

    pub fn shared(responder: Arc<dyn MemWsResponder>) -> Arc<dyn WsTransport> {
        Arc::new(Self::new(responder))
    }

    pub fn shared_with_capacity(
        responder: Arc<dyn MemWsResponder>,
        capacity: usize,
    ) -> Arc<dyn WsTransport> {
        Arc::new(Self::with_capacity(responder, capacity))
    }
}

impl WsTransport for MemWsTransport {
    fn connect(&self, url: Url) -> WsConnectFuture {
        let responder = self.responder.clone();
        let capacity = self.capacity;
        Box::pin(async move {
            let (c2s_tx, c2s_rx) = mpsc::unbounded_channel();
            let (s2c_tx, s2c_rx) = mpsc::channel(capacity);
            let server_future = responder.spawn_server(url, c2s_rx, s2c_tx);
            tokio::spawn(server_future);
            let sink: Box<dyn WsSink> = Box::new(MemWsSink { sender: c2s_tx });
            let stream: Box<dyn WsStream> = Box::new(MemWsStream { receiver: s2c_rx });
            Ok(WsConn { sink, stream })
        })
    }
}

struct MemWsSink {
    sender: mpsc::UnboundedSender<WsMessage>,
}

impl WsSink for MemWsSink {
    fn send<'a>(&'a mut self, message: WsMessage) -> WsSendFuture<'a> {
        let result = self
            .sender
            .send(message)
            .map_err(|_| NetworkError::Transport("server side closed channel".into()));
        Box::pin(async move { result })
    }
}

struct MemWsStream {
    receiver: mpsc::Receiver<WsMessage>,
}

impl WsStream for MemWsStream {
    fn next<'a>(&'a mut self) -> WsMessageFuture<'a> {
        Box::pin(async move { self.receiver.recv().await.map(Ok) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SimClock;
    use crate::UnixMicros;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::Instant;

    struct ScriptedHttp {
        responses: Mutex<Vec<MemHttpResponse>>,
        cursor: AtomicUsize,
    }

    impl ScriptedHttp {
        fn new(responses: Vec<MemHttpResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
                cursor: AtomicUsize::new(0),
            }
        }
    }

    impl MemHttpResponder for ScriptedHttp {
        fn respond(&self, _: &HttpRequest) -> MemHttpResponse {
            let i = self.cursor.fetch_add(1, Ordering::Relaxed);
            let mut guard = self.responses.lock().unwrap();
            std::mem::replace(
                &mut guard[i],
                MemHttpResponse {
                    latency: Duration::ZERO,
                    result: Err(NetworkError::Transport("script consumed".into())),
                },
            )
        }
    }

    fn ok_response(body: &str, latency_ms: u64) -> MemHttpResponse {
        MemHttpResponse {
            latency: Duration::from_millis(latency_ms),
            result: Ok(MemHttpBody::ok_json(Bytes::from(body.to_owned()))),
        }
    }

    fn err_response(latency_ms: u64) -> MemHttpResponse {
        MemHttpResponse {
            latency: Duration::from_millis(latency_ms),
            result: Err(NetworkError::Transport("brownout".into())),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn http_returns_scripted_body_after_injected_latency() {
        let clock: Arc<dyn Clock> = Arc::new(SimClock::at(UnixMicros::new(0)));
        let responder: Arc<dyn MemHttpResponder> =
            Arc::new(ScriptedHttp::new(vec![ok_response("{\"hello\":1}", 10)]));
        let transport = MemHttpTransport::new(responder, clock);
        let request = HttpRequest {
            url: Url::parse("http://oyster.cafe/xrpc/x").unwrap(),
            headers: HeaderMap::new(),
        };

        let before = Instant::now();
        let resp = transport.execute(request).await.unwrap();
        let elapsed = Instant::now().saturating_duration_since(before);

        assert_eq!(resp.status, StatusCode::OK);
        assert_eq!(elapsed, Duration::from_millis(10));
        let chunks = collect_body(resp.body).await;
        assert_eq!(chunks.as_ref(), b"{\"hello\":1}");
    }

    #[tokio::test(start_paused = true)]
    async fn http_propagates_scripted_errors_with_latency() {
        let clock: Arc<dyn Clock> = Arc::new(SimClock::at(UnixMicros::new(0)));
        let responder: Arc<dyn MemHttpResponder> =
            Arc::new(ScriptedHttp::new(vec![err_response(50)]));
        let transport = MemHttpTransport::new(responder, clock);
        let request = HttpRequest {
            url: Url::parse("http://oyster.cafe/xrpc/x").unwrap(),
            headers: HeaderMap::new(),
        };

        let before = Instant::now();
        let resp = transport.execute(request).await;
        let elapsed = Instant::now().saturating_duration_since(before);

        assert_eq!(elapsed, Duration::from_millis(50));
        assert!(matches!(resp, Err(NetworkError::Transport(_))));
    }

    async fn collect_body(mut body: BodyStream) -> Bytes {
        use futures::StreamExt;
        let mut acc = Vec::new();
        while let Some(chunk) = body.next().await {
            acc.extend_from_slice(&chunk.unwrap());
        }
        Bytes::from(acc)
    }

    struct ScriptedWs {
        frames: Mutex<Vec<WsMessage>>,
    }

    impl MemWsResponder for ScriptedWs {
        fn spawn_server(
            &self,
            _: Url,
            mut _recv: mpsc::UnboundedReceiver<WsMessage>,
            send: mpsc::Sender<WsMessage>,
        ) -> MemWsServerFuture {
            let frames: Vec<WsMessage> = std::mem::take(&mut *self.frames.lock().unwrap());
            Box::pin(async move {
                for frame in frames {
                    if send.send(frame).await.is_err() {
                        return;
                    }
                }
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ws_delivers_scripted_frames_to_client() {
        let responder: Arc<dyn MemWsResponder> = Arc::new(ScriptedWs {
            frames: Mutex::new(vec![
                WsMessage::Text("frame-a".into()),
                WsMessage::Text("frame-b".into()),
            ]),
        });
        let transport = MemWsTransport::new(responder);
        let mut conn = transport
            .connect(Url::parse("ws://oyster.cafe/").unwrap())
            .await
            .unwrap();
        let a = conn.stream.next().await.unwrap().unwrap();
        let b = conn.stream.next().await.unwrap().unwrap();
        assert!(matches!(a, WsMessage::Text(t) if t == "frame-a"));
        assert!(matches!(b, WsMessage::Text(t) if t == "frame-b"));
    }

    struct EchoWs;

    impl MemWsResponder for EchoWs {
        fn spawn_server(
            &self,
            _: Url,
            mut recv: mpsc::UnboundedReceiver<WsMessage>,
            send: mpsc::Sender<WsMessage>,
        ) -> MemWsServerFuture {
            Box::pin(async move {
                while let Some(msg) = recv.recv().await {
                    if send.send(msg).await.is_err() {
                        return;
                    }
                }
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ws_round_trip_via_server_echo() {
        let transport = MemWsTransport::new(Arc::new(EchoWs));
        let mut conn = transport
            .connect(Url::parse("ws://oyster.cafe/").unwrap())
            .await
            .unwrap();
        conn.sink
            .send(WsMessage::Text("ping".into()))
            .await
            .unwrap();
        let echoed = conn.stream.next().await.unwrap().unwrap();
        assert!(matches!(echoed, WsMessage::Text(t) if t == "ping"));
    }
}
