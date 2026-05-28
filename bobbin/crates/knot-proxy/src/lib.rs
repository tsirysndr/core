use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bobbin_runtime::{
    BodyStream as InnerBodyStream, Clock, HttpRequest, HttpResponseHead, HttpTransport,
    NetworkError, ReqwestHttp, RuntimeHasher,
};
use bytes::Bytes;
use futures::Stream;
use http::{HeaderMap, StatusCode};
use jacquard_common::BosStr;
use jacquard_common::types::nsid::Nsid;
use reqwest::{Client, redirect::Policy};
use scc::HashMap as SccMap;
use thiserror::Error;
use url::Url;

mod breaker;
mod dns;
mod host;

pub use breaker::{Breaker, BreakerPermit, CircuitOpen, FailureThreshold, ThresholdError};
pub use host::{KnotHost, KnotHostError, PrivateHostReason, RepoSlug, RepoSlugError};

const USER_AGENT: &str = concat!("bobbin/", env!("CARGO_PKG_VERSION"));
const HTTPS_SCHEME: &str = "https";

#[derive(Clone, Debug)]
pub struct KnotProxyConfig {
    pub failure_threshold: FailureThreshold,
    pub cooldown: Duration,
    pub allow_private_hosts: bool,
    pub require_https: bool,
}

impl Default for KnotProxyConfig {
    fn default() -> Self {
        Self {
            failure_threshold: FailureThreshold::new(5).expect("nonzero literal"),
            cooldown: Duration::from_secs(30),
            allow_private_hosts: false,
            require_https: true,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct KnotHttpConfig {
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
}

impl Default for KnotHttpConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            read_timeout: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Error)]
pub enum KnotProxyError {
    #[error("circuit breaker open")]
    CircuitOpen,
    #[error("blocked: host {host} resolves to {reason} address space")]
    BlockedHost {
        host: String,
        reason: PrivateHostReason,
    },
    #[error("blocked: knot {host} requires https, got plaintext http")]
    PlaintextHttp { host: String },
    #[error("connect failed: {0}")]
    Connect(String),
    #[error("upstream read timed out: {0}")]
    Timeout(String),
    #[error("redirect refused: {0}")]
    Redirect(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("upstream returned status {0}")]
    Upstream(StatusCode),
}

pub struct KnotProxy {
    http: Arc<dyn HttpTransport>,
    breakers: SccMap<KnotHost, Arc<Breaker>, RuntimeHasher>,
    threshold: FailureThreshold,
    cooldown: Duration,
    allow_private_hosts: bool,
    require_https: bool,
    clock: Arc<dyn Clock>,
}

impl KnotProxy {
    pub fn new(
        config: KnotProxyConfig,
        http: KnotHttpConfig,
        clock: Arc<dyn Clock>,
        hasher: RuntimeHasher,
    ) -> Result<Self, reqwest::Error> {
        let resolver = Arc::new(dns::PrivateAddressFilter::new(config.allow_private_hosts));
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(http.connect_timeout)
            .read_timeout(http.read_timeout)
            .redirect(Policy::none())
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .dns_resolver(resolver)
            .build()?;
        Ok(Self::with_transport(
            ReqwestHttp::shared(client),
            config,
            clock,
            hasher,
        ))
    }

    pub fn with_transport(
        http: Arc<dyn HttpTransport>,
        config: KnotProxyConfig,
        clock: Arc<dyn Clock>,
        hasher: RuntimeHasher,
    ) -> Self {
        Self {
            http,
            breakers: SccMap::with_hasher(hasher),
            threshold: config.failure_threshold,
            cooldown: config.cooldown,
            allow_private_hosts: config.allow_private_hosts,
            require_https: config.require_https,
            clock,
        }
    }

    pub fn allows_private_hosts(&self) -> bool {
        self.allow_private_hosts
    }

    pub fn requires_https(&self) -> bool {
        self.require_https
    }

    pub async fn forward<S: BosStr + AsRef<str>>(
        &self,
        host: &KnotHost,
        nsid: &Nsid<S>,
        query: &[(&str, &str)],
        headers: HeaderMap,
    ) -> Result<ProxyResponse, KnotProxyError> {
        self.guard_host(host)?;
        let breaker = self.breaker_for(host).await;
        let permit = breaker
            .try_acquire()
            .map_err(|_: CircuitOpen| KnotProxyError::CircuitOpen)?;
        let url = build_xrpc_url(host, nsid, query);
        let outcome = self.http.execute(HttpRequest { url, headers }).await;
        classify(outcome, permit)
    }

    fn guard_host(&self, host: &KnotHost) -> Result<(), KnotProxyError> {
        let host_str = || host.url().host_str().unwrap_or_default().to_owned();
        if self.require_https && host.url().scheme() != HTTPS_SCHEME {
            return Err(KnotProxyError::PlaintextHttp { host: host_str() });
        }
        if self.allow_private_hosts {
            return Ok(());
        }
        match host.private_literal_reason() {
            None => Ok(()),
            Some(reason) => Err(KnotProxyError::BlockedHost {
                host: host_str(),
                reason,
            }),
        }
    }

    async fn breaker_for(&self, host: &KnotHost) -> Arc<Breaker> {
        if let Some(existing) = self.breakers.read_async(host, |_, v| Arc::clone(v)).await {
            return existing;
        }
        let entry = self.breakers.entry_async(host.clone()).await;
        Arc::clone(
            entry
                .or_insert_with(|| {
                    Arc::new(Breaker::new(
                        self.threshold,
                        self.cooldown,
                        self.clock.clone(),
                    ))
                })
                .get(),
        )
    }
}

pub struct ProxyResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: InnerBodyStream,
    permit: BreakerPermit,
}

impl std::fmt::Debug for ProxyResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .finish_non_exhaustive()
    }
}

impl ProxyResponse {
    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub fn into_body_stream(self) -> BodyStream {
        BodyStream::new(self.body, self.permit)
    }
}

pub struct BodyStream {
    inner: InnerBodyStream,
    permit: Option<BreakerPermit>,
}

impl BodyStream {
    fn new(inner: InnerBodyStream, permit: BreakerPermit) -> Self {
        Self {
            inner,
            permit: Some(permit),
        }
    }

    fn resolve(&mut self, success: bool) {
        if let Some(permit) = self.permit.take() {
            if success {
                permit.record_success();
            } else {
                permit.record_failure();
            }
        }
    }
}

impl Stream for BodyStream {
    type Item = Result<Bytes, NetworkError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let next = match self.inner.as_mut().poll_next(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(item) => item,
        };
        match &next {
            Some(Ok(_)) => {}
            Some(Err(_)) => self.resolve(false),
            None => self.resolve(true),
        }
        Poll::Ready(next)
    }
}

fn build_xrpc_url<S: BosStr + AsRef<str>>(
    host: &KnotHost,
    nsid: &Nsid<S>,
    query: &[(&str, &str)],
) -> Url {
    let mut url = host.xrpc_url(nsid);
    {
        let mut pairs = url.query_pairs_mut();
        query.iter().for_each(|(k, v)| {
            pairs.append_pair(k, v);
        });
    }
    url
}

fn classify(
    outcome: Result<HttpResponseHead, NetworkError>,
    permit: BreakerPermit,
) -> Result<ProxyResponse, KnotProxyError> {
    match outcome {
        Ok(head) if is_upstream_failure(head.status) => {
            let status = head.status;
            permit.record_failure();
            Err(KnotProxyError::Upstream(status))
        }
        Ok(head) => Ok(ProxyResponse {
            status: head.status,
            headers: head.headers,
            body: head.body,
            permit,
        }),
        Err(err) => {
            permit.record_failure();
            Err(map_network(err))
        }
    }
}

fn is_upstream_failure(status: StatusCode) -> bool {
    status.is_server_error() || is_unfollowable_redirect(status)
}

fn is_unfollowable_redirect(status: StatusCode) -> bool {
    matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308)
}

fn map_network(err: NetworkError) -> KnotProxyError {
    match err {
        NetworkError::Timeout(msg) => KnotProxyError::Timeout(msg),
        NetworkError::Connect(msg) => KnotProxyError::Connect(msg),
        NetworkError::Redirect(msg) => KnotProxyError::Redirect(msg),
        NetworkError::Transport(msg) | NetworkError::Body(msg) | NetworkError::Protocol(msg) => {
            KnotProxyError::Transport(msg)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobbin_runtime::SystemClock;
    use futures::stream::TryStreamExt;
    use jacquard_common::DefaultStr;
    use tokio::io::AsyncWriteExt;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn nsid(s: &'static str) -> Nsid<DefaultStr> {
        Nsid::new_static(s).unwrap()
    }

    pub(crate) fn config_for_test() -> KnotProxyConfig {
        KnotProxyConfig {
            failure_threshold: FailureThreshold::new(2).unwrap(),
            cooldown: Duration::from_millis(80),
            allow_private_hosts: true,
            require_https: false,
        }
    }

    pub(crate) fn http_config_for_test() -> KnotHttpConfig {
        KnotHttpConfig {
            connect_timeout: Duration::from_millis(500),
            read_timeout: Duration::from_secs(2),
        }
    }

    fn proxy_for_test() -> KnotProxy {
        KnotProxy::new(
            config_for_test(),
            http_config_for_test(),
            Arc::new(SystemClock::new()),
            RuntimeHasher::default(),
        )
        .unwrap()
    }

    async fn server() -> MockServer {
        MockServer::start().await
    }

    fn host_of(server: &MockServer) -> KnotHost {
        KnotHost::parse(&server.uri()).unwrap()
    }

    pub(crate) async fn drain(stream: BodyStream) -> Result<Bytes, NetworkError> {
        let chunks: Vec<Bytes> = stream.try_collect().await?;
        let total: usize = chunks.iter().map(|b| b.len()).sum();
        let mut buf = bytes::BytesMut::with_capacity(total);
        chunks.iter().for_each(|c| buf.extend_from_slice(c));
        Ok(buf.freeze())
    }

    #[tokio::test]
    async fn forwards_query_params_and_returns_body() {
        let server = server().await;
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.repo.blob"))
            .and(query_param("repo", "did:plc:squid/barnacle"))
            .and(query_param("ref", "main"))
            .and(query_param("path", "README.md"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(r#"{"path":"README.md"}"#),
            )
            .mount(&server)
            .await;

        let proxy = proxy_for_test();
        let resp = proxy
            .forward(
                &host_of(&server),
                &nsid("sh.tangled.repo.blob"),
                &[
                    ("repo", "did:plc:squid/barnacle"),
                    ("ref", "main"),
                    ("path", "README.md"),
                ],
                HeaderMap::new(),
            )
            .await
            .expect("happy path");
        assert_eq!(resp.status(), 200);
        let body = drain(resp.into_body_stream()).await.unwrap();
        assert_eq!(&body[..], br#"{"path":"README.md"}"#);
    }

    #[tokio::test]
    async fn five_hundreds_open_breaker() {
        let server = server().await;
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.repo.blob"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let proxy = proxy_for_test();
        let host = host_of(&server);
        let r1 = proxy
            .forward(&host, &nsid("sh.tangled.repo.blob"), &[], HeaderMap::new())
            .await;
        assert!(matches!(r1, Err(KnotProxyError::Upstream(_))));
        let r2 = proxy
            .forward(&host, &nsid("sh.tangled.repo.blob"), &[], HeaderMap::new())
            .await;
        assert!(matches!(r2, Err(KnotProxyError::Upstream(_))));
        let r3 = proxy
            .forward(&host, &nsid("sh.tangled.repo.blob"), &[], HeaderMap::new())
            .await;
        assert!(matches!(r3, Err(KnotProxyError::CircuitOpen)));
    }

    #[tokio::test]
    async fn four_hundreds_do_not_open_breaker() {
        let server = server().await;
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.repo.blob"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let proxy = proxy_for_test();
        let host = host_of(&server);
        let r1 = proxy
            .forward(&host, &nsid("sh.tangled.repo.blob"), &[], HeaderMap::new())
            .await;
        assert_eq!(r1.unwrap().status(), 404);
        let r2 = proxy
            .forward(&host, &nsid("sh.tangled.repo.blob"), &[], HeaderMap::new())
            .await;
        assert_eq!(r2.unwrap().status(), 404);
        let r3 = proxy
            .forward(&host, &nsid("sh.tangled.repo.blob"), &[], HeaderMap::new())
            .await;
        assert_eq!(
            r3.unwrap().status(),
            404,
            "client errors must not trip breaker",
        );
    }

    #[tokio::test]
    async fn breaker_recovers_after_cooldown() {
        let server = server().await;
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.repo.blob"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.repo.blob"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string("ok"),
            )
            .mount(&server)
            .await;

        let proxy = proxy_for_test();
        let host = host_of(&server);
        let _ = proxy
            .forward(&host, &nsid("sh.tangled.repo.blob"), &[], HeaderMap::new())
            .await;
        let _ = proxy
            .forward(&host, &nsid("sh.tangled.repo.blob"), &[], HeaderMap::new())
            .await;
        assert!(matches!(
            proxy
                .forward(&host, &nsid("sh.tangled.repo.blob"), &[], HeaderMap::new())
                .await,
            Err(KnotProxyError::CircuitOpen),
        ));
        tokio::time::sleep(Duration::from_millis(120)).await;
        let recovered = proxy
            .forward(&host, &nsid("sh.tangled.repo.blob"), &[], HeaderMap::new())
            .await
            .expect("must recover after cooldown");
        assert_eq!(recovered.status(), 200);
        let body = drain(recovered.into_body_stream()).await.unwrap();
        assert_eq!(&body[..], b"ok");
    }

    #[tokio::test]
    async fn breakers_are_isolated_per_host() {
        let bad = server().await;
        let good = server().await;
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.repo.blob"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&bad)
            .await;
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.repo.blob"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string("ok"),
            )
            .mount(&good)
            .await;

        let proxy = proxy_for_test();
        let bad_host = host_of(&bad);
        let good_host = host_of(&good);
        let _ = proxy
            .forward(
                &bad_host,
                &nsid("sh.tangled.repo.blob"),
                &[],
                HeaderMap::new(),
            )
            .await;
        let _ = proxy
            .forward(
                &bad_host,
                &nsid("sh.tangled.repo.blob"),
                &[],
                HeaderMap::new(),
            )
            .await;
        assert!(matches!(
            proxy
                .forward(
                    &bad_host,
                    &nsid("sh.tangled.repo.blob"),
                    &[],
                    HeaderMap::new()
                )
                .await,
            Err(KnotProxyError::CircuitOpen),
        ));
        let resp = proxy
            .forward(
                &good_host,
                &nsid("sh.tangled.repo.blob"),
                &[],
                HeaderMap::new(),
            )
            .await
            .expect("healthy host stays open");
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn build_xrpc_url_appends_query() {
        let host = KnotHost::parse("https://oyster.cafe").unwrap();
        let url = build_xrpc_url(
            &host,
            &nsid("sh.tangled.repo.tree"),
            &[("repo", "did:plc:squid/barnacle"), ("ref", "main")],
        );
        assert_eq!(
            url.as_str(),
            "https://oyster.cafe/xrpc/sh.tangled.repo.tree?repo=did%3Aplc%3Asquid%2Fbarnacle&ref=main",
        );
    }

    #[tokio::test]
    async fn rejects_private_host_by_default() {
        let server = server().await;
        let strict = KnotProxyConfig {
            allow_private_hosts: false,
            ..config_for_test()
        };
        let proxy = KnotProxy::new(
            strict,
            http_config_for_test(),
            Arc::new(SystemClock::new()),
            RuntimeHasher::default(),
        )
        .unwrap();
        let err = proxy
            .forward(
                &host_of(&server),
                &nsid("sh.tangled.repo.blob"),
                &[],
                HeaderMap::new(),
            )
            .await
            .expect_err("loopback must be blocked under strict config");
        assert!(matches!(err, KnotProxyError::BlockedHost { .. }));
    }

    #[tokio::test]
    async fn rejects_plaintext_when_https_required() {
        let server = server().await;
        let strict = KnotProxyConfig {
            require_https: true,
            ..config_for_test()
        };
        let proxy = KnotProxy::new(
            strict,
            http_config_for_test(),
            Arc::new(SystemClock::new()),
            RuntimeHasher::default(),
        )
        .unwrap();
        let err = proxy
            .forward(
                &host_of(&server),
                &nsid("sh.tangled.repo.blob"),
                &[],
                HeaderMap::new(),
            )
            .await
            .expect_err("plaintext http must be rejected under https-required");
        assert!(
            matches!(err, KnotProxyError::PlaintextHttp { .. }),
            "got {err:?}",
        );
    }

    #[tokio::test]
    async fn transport_error_trips_breaker() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let dead = KnotHost::parse(&format!("http://{addr}")).unwrap();

        let proxy = proxy_for_test();
        let r1 = proxy
            .forward(&dead, &nsid("sh.tangled.repo.blob"), &[], HeaderMap::new())
            .await;
        assert!(
            r1.is_err(),
            "transport must fail against closed port: {r1:?}"
        );
        let r2 = proxy
            .forward(&dead, &nsid("sh.tangled.repo.blob"), &[], HeaderMap::new())
            .await;
        assert!(r2.is_err(), "second transport must fail: {r2:?}");
        let r3 = proxy
            .forward(&dead, &nsid("sh.tangled.repo.blob"), &[], HeaderMap::new())
            .await;
        assert!(
            matches!(r3, Err(KnotProxyError::CircuitOpen)),
            "transport failures must trip breaker, got {r3:?}",
        );
    }

    #[tokio::test]
    async fn redirects_surface_as_upstream_failure() {
        let primary = server().await;
        let secondary = server().await;
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.repo.blob"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", &format!("{}/secret", secondary.uri())),
            )
            .mount(&primary)
            .await;
        Mock::given(method("GET"))
            .and(path("/secret"))
            .respond_with(ResponseTemplate::new(200).set_body_string("leaked"))
            .mount(&secondary)
            .await;

        let proxy = proxy_for_test();
        let err = proxy
            .forward(
                &host_of(&primary),
                &nsid("sh.tangled.repo.blob"),
                &[],
                HeaderMap::new(),
            )
            .await
            .expect_err("302 must surface as upstream failure");
        assert!(
            matches!(err, KnotProxyError::Upstream(s) if s.as_u16() == 302),
            "got {err:?}",
        );
        let received = secondary.received_requests().await.unwrap();
        assert!(received.is_empty(), "secondary must never be dialled");
    }

    #[tokio::test]
    async fn not_modified_passes_through() {
        let server = server().await;
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.repo.blob"))
            .respond_with(ResponseTemplate::new(304).insert_header("etag", "\"v1\""))
            .mount(&server)
            .await;
        let proxy = proxy_for_test();
        let resp = proxy
            .forward(
                &host_of(&server),
                &nsid("sh.tangled.repo.blob"),
                &[],
                HeaderMap::new(),
            )
            .await
            .expect("304 is a cache validator, not a redirect");
        assert_eq!(resp.status(), 304);
    }

    #[tokio::test]
    async fn mid_stream_drop_records_breaker_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            async fn drop_after_partial(mut socket: tokio::net::TcpStream) {
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 1024\r\nContent-Type: application/octet-stream\r\n\r\nabcd",
                    )
                    .await;
                drop(socket);
            }
            let admit = || async {
                let (socket, _) = listener.accept().await.ok()?;
                drop_after_partial(socket).await;
                Some(())
            };
            admit().await;
            admit().await;
        });

        let host = KnotHost::parse(&format!("http://{addr}")).unwrap();
        let proxy = proxy_for_test();

        let r1 = proxy
            .forward(&host, &nsid("sh.tangled.repo.blob"), &[], HeaderMap::new())
            .await
            .expect("headers arrive even when body is truncated");
        assert_eq!(r1.status(), 200);
        let _ = drain(r1.into_body_stream()).await;

        let r2 = proxy
            .forward(&host, &nsid("sh.tangled.repo.blob"), &[], HeaderMap::new())
            .await
            .expect("second call still gets headers");
        let _ = drain(r2.into_body_stream()).await;

        let r3 = proxy
            .forward(&host, &nsid("sh.tangled.repo.blob"), &[], HeaderMap::new())
            .await;
        assert!(
            matches!(r3, Err(KnotProxyError::CircuitOpen)),
            "two truncated streams must trip the breaker, got {r3:?}",
        );
        server.abort();
    }
}
