use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures::stream::{Stream, StreamExt};
use http::{HeaderMap, StatusCode};
use thiserror::Error;
use tokio_tungstenite::tungstenite::{
    Bytes as WsBytes, Message as TungsteniteMessage, protocol::CloseFrame as TungsteniteClose,
    protocol::frame::coding::CloseCode as TungsteniteCloseCode,
};
use url::Url;

#[derive(Debug, Error)]
pub enum NetworkError {
    #[error("connect: {0}")]
    Connect(String),
    #[error("timeout: {0}")]
    Timeout(String),
    #[error("redirect: {0}")]
    Redirect(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("body: {0}")]
    Body(String),
    #[error("protocol: {0}")]
    Protocol(String),
}

pub struct HttpRequest {
    pub url: Url,
    pub headers: HeaderMap,
}

pub type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, NetworkError>> + Send + 'static>>;

pub struct HttpResponseHead {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub content_length: Option<u64>,
    pub body: BodyStream,
}

pub type HttpResult = Result<HttpResponseHead, NetworkError>;
pub type HttpResponseFuture = Pin<Box<dyn Future<Output = HttpResult> + Send + 'static>>;

pub trait HttpTransport: Send + Sync + 'static {
    fn execute(&self, request: HttpRequest) -> HttpResponseFuture;
}

#[derive(Clone, Debug)]
pub struct ReqwestHttp {
    client: reqwest::Client,
}

impl ReqwestHttp {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }

    pub fn shared(client: reqwest::Client) -> Arc<dyn HttpTransport> {
        Arc::new(Self::new(client))
    }
}

impl HttpTransport for ReqwestHttp {
    fn execute(&self, request: HttpRequest) -> HttpResponseFuture {
        let client = self.client.clone();
        Box::pin(async move {
            let resp = client
                .get(request.url)
                .headers(request.headers)
                .send()
                .await
                .map_err(map_reqwest)?;
            let status = resp.status();
            let headers = resp.headers().clone();
            let content_length = resp.content_length();
            let body: BodyStream = Box::pin(
                resp.bytes_stream()
                    .map(|chunk| chunk.map_err(|e| NetworkError::Body(e.to_string()))),
            );
            Ok(HttpResponseHead {
                status,
                headers,
                content_length,
                body,
            })
        })
    }
}

fn map_reqwest(err: reqwest::Error) -> NetworkError {
    let msg = err.to_string();
    if err.is_timeout() {
        NetworkError::Timeout(msg)
    } else if err.is_connect() {
        NetworkError::Connect(msg)
    } else if err.is_redirect() {
        NetworkError::Redirect(msg)
    } else {
        NetworkError::Transport(msg)
    }
}

#[derive(Clone, Debug)]
pub enum WsMessage {
    Text(String),
    Binary(Bytes),
    Ping(Bytes),
    Pong(Bytes),
    Close { code: u16, reason: String },
}

pub type WsSendFuture<'a> = Pin<Box<dyn Future<Output = Result<(), NetworkError>> + Send + 'a>>;
pub type WsMessageFuture<'a> =
    Pin<Box<dyn Future<Output = Option<Result<WsMessage, NetworkError>>> + Send + 'a>>;

pub trait WsSink: Send + 'static {
    fn send<'a>(&'a mut self, message: WsMessage) -> WsSendFuture<'a>;
}

pub trait WsStream: Send + 'static {
    fn next<'a>(&'a mut self) -> WsMessageFuture<'a>;
}

pub struct WsConn {
    pub sink: Box<dyn WsSink>,
    pub stream: Box<dyn WsStream>,
}

pub type WsConnectFuture =
    Pin<Box<dyn Future<Output = Result<WsConn, NetworkError>> + Send + 'static>>;

pub trait WsTransport: Send + Sync + 'static {
    fn connect(&self, url: Url) -> WsConnectFuture;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TungsteniteWs;

impl TungsteniteWs {
    pub fn shared() -> Arc<dyn WsTransport> {
        Arc::new(Self)
    }
}

impl WsTransport for TungsteniteWs {
    fn connect(&self, url: Url) -> WsConnectFuture {
        Box::pin(async move {
            let url_str = url.as_str().to_owned();
            let (ws, _resp) = tokio_tungstenite::connect_async(&url_str)
                .await
                .map_err(|e| NetworkError::Connect(e.to_string()))?;
            let (sink_inner, stream_inner) = futures::StreamExt::split(ws);
            let sink: Box<dyn WsSink> = Box::new(TungsteniteSink { inner: sink_inner });
            let stream: Box<dyn WsStream> = Box::new(TungsteniteStream {
                inner: stream_inner,
            });
            Ok(WsConn { sink, stream })
        })
    }
}

type TungsteniteWsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct TungsteniteSink {
    inner: futures::stream::SplitSink<TungsteniteWsStream, TungsteniteMessage>,
}

impl WsSink for TungsteniteSink {
    fn send<'a>(&'a mut self, message: WsMessage) -> WsSendFuture<'a> {
        Box::pin(async move {
            use futures::SinkExt;
            self.inner
                .send(message_to_tungstenite(message))
                .await
                .map_err(|e| NetworkError::Transport(e.to_string()))
        })
    }
}

struct TungsteniteStream {
    inner: futures::stream::SplitStream<TungsteniteWsStream>,
}

impl WsStream for TungsteniteStream {
    fn next<'a>(&'a mut self) -> WsMessageFuture<'a> {
        Box::pin(async move {
            let item = StreamExt::next(&mut self.inner).await?;
            Some(
                item.map_err(|e| NetworkError::Transport(e.to_string()))
                    .and_then(message_from_tungstenite),
            )
        })
    }
}

fn message_to_tungstenite(message: WsMessage) -> TungsteniteMessage {
    match message {
        WsMessage::Text(text) => TungsteniteMessage::Text(text.into()),
        WsMessage::Binary(bytes) => TungsteniteMessage::Binary(WsBytes::copy_from_slice(&bytes)),
        WsMessage::Ping(bytes) => TungsteniteMessage::Ping(WsBytes::copy_from_slice(&bytes)),
        WsMessage::Pong(bytes) => TungsteniteMessage::Pong(WsBytes::copy_from_slice(&bytes)),
        WsMessage::Close { code, reason } => TungsteniteMessage::Close(Some(TungsteniteClose {
            code: TungsteniteCloseCode::from(code),
            reason: reason.into(),
        })),
    }
}

fn message_from_tungstenite(message: TungsteniteMessage) -> Result<WsMessage, NetworkError> {
    match message {
        TungsteniteMessage::Text(t) => Ok(WsMessage::Text(t.to_string())),
        TungsteniteMessage::Binary(b) => Ok(WsMessage::Binary(Bytes::copy_from_slice(&b))),
        TungsteniteMessage::Ping(b) => Ok(WsMessage::Ping(Bytes::copy_from_slice(&b))),
        TungsteniteMessage::Pong(b) => Ok(WsMessage::Pong(Bytes::copy_from_slice(&b))),
        TungsteniteMessage::Close(close) => {
            let (code, reason) = close
                .map(|c| (u16::from(c.code), c.reason.to_string()))
                .unwrap_or((1000, String::new()));
            Ok(WsMessage::Close { code, reason })
        }
        TungsteniteMessage::Frame(_) => Err(NetworkError::Protocol(
            "tungstenite raw frame surfaced unexpectedly".to_owned(),
        )),
    }
}
