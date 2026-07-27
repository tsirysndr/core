use std::net::{IpAddr, SocketAddr};
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::error_handling::HandleErrorLayer;
use axum::extract::{ConnectInfo, State};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use governor::middleware::NoOpMiddleware;
use http::{HeaderName, Method, Request, StatusCode};
use tokio_util::sync::CancellationToken;
use tower::limit::GlobalConcurrencyLimitLayer;
use tower::load_shed::LoadShedLayer;
use tower::{BoxError, ServiceBuilder};
use tower_governor::GovernorLayer;
use tower_governor::errors::GovernorError;
use tower_governor::governor::{GovernorConfig, GovernorConfigBuilder};
use tower_governor::key_extractor::KeyExtractor;
use tower_http::map_request_body::MapRequestBodyLayer;
use tower_http::timeout::{RequestBodyTimeoutLayer, TimeoutBody};

const NANOS_PER_SECOND: u64 = 1_000_000_000;
const CLEANUP_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy)]
pub struct RequestsPerSecond(NonZeroU32);

impl RequestsPerSecond {
    pub fn new(value: NonZeroU32) -> Self {
        Self(value)
    }

    fn period(self) -> Duration {
        Duration::from_nanos((NANOS_PER_SECOND / u64::from(self.0.get())).max(1))
    }
}

knot_types::scalar_newtype! {
    pub struct BurstSize(NonZeroU32);
    pub struct MaxInflightRequests(NonZeroU32);
}

#[derive(Debug, Clone, Copy)]
pub struct RequestTimeout(Duration);

impl RequestTimeout {
    pub fn from_millis(millis: NonZeroU64) -> Self {
        Self(Duration::from_millis(millis.get()))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BodyInactivityTimeout(Duration);

impl BodyInactivityTimeout {
    pub fn from_millis(millis: NonZeroU64) -> Self {
        Self(Duration::from_millis(millis.get()))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WriteRequestTimeout(Duration);

impl WriteRequestTimeout {
    pub fn from_millis(millis: NonZeroU64) -> Self {
        Self(Duration::from_millis(millis.get()))
    }
}

pub struct EdgeGuards {
    rate: RequestsPerSecond,
    burst: BurstSize,
    max_inflight: MaxInflightRequests,
    request_timeout: RequestTimeout,
    body_timeout: BodyInactivityTimeout,
    write_request_timeout: WriteRequestTimeout,
    proxy_header: Option<HeaderName>,
}

impl EdgeGuards {
    pub fn new(
        rate: RequestsPerSecond,
        burst: BurstSize,
        max_inflight: MaxInflightRequests,
        request_timeout: RequestTimeout,
        body_timeout: BodyInactivityTimeout,
        write_request_timeout: WriteRequestTimeout,
        proxy_header: Option<HeaderName>,
    ) -> Self {
        Self {
            rate,
            burst,
            max_inflight,
            request_timeout,
            body_timeout,
            write_request_timeout,
            proxy_header,
        }
    }

    pub(crate) fn prepare(self, shutdown: &CancellationToken) -> GuardLayers {
        let governor = build_governor(self.rate, self.burst, self.proxy_header);
        spawn_state_cleanup(Arc::clone(&governor), shutdown.clone());
        GuardLayers {
            governor,
            max_inflight: self.max_inflight.0.get() as usize,
            body_timeout: self.body_timeout.0,
            timeout: TimeoutBudget {
                standard: self.request_timeout.0,
                extended: self.write_request_timeout.0,
            },
        }
    }
}

type GuardGovernor = GovernorConfig<ProxyAwareIp, NoOpMiddleware>;

pub(crate) struct GuardLayers {
    governor: Arc<GuardGovernor>,
    max_inflight: usize,
    body_timeout: Duration,
    timeout: TimeoutBudget,
}

#[derive(Clone, Copy)]
struct TimeoutBudget {
    standard: Duration,
    extended: Duration,
}

#[derive(Clone)]
struct ProxyAwareIp {
    header: Option<HeaderName>,
}

impl KeyExtractor for ProxyAwareIp {
    type Key = IpAddr;

    fn extract<T>(&self, request: &Request<T>) -> Result<Self::Key, GovernorError> {
        let from_header = self
            .header
            .as_ref()
            .and_then(|header| knot_types::forwarded_peer(request.headers(), header));
        from_header
            .or_else(|| {
                request
                    .extensions()
                    .get::<ConnectInfo<SocketAddr>>()
                    .map(|info| info.0.ip())
            })
            .ok_or(GovernorError::UnableToExtractKey)
    }
}

fn build_governor(
    rate: RequestsPerSecond,
    burst: BurstSize,
    proxy_header: Option<HeaderName>,
) -> Arc<GuardGovernor> {
    let mut builder = GovernorConfigBuilder::default();
    builder.period(rate.period()).burst_size(burst.0.get());
    let config = builder
        .key_extractor(ProxyAwareIp {
            header: proxy_header,
        })
        .finish()
        .expect("a non-zero rate period and burst size always yield a governor config");
    Arc::new(config)
}

fn spawn_state_cleanup(governor: Arc<GuardGovernor>, shutdown: CancellationToken) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(CLEANUP_INTERVAL);
        ticker.tick().await;
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                _ = ticker.tick() => governor.limiter().retain_recent(),
            }
        }
    });
}

async fn shed_overloaded(_error: BoxError) -> StatusCode {
    StatusCode::SERVICE_UNAVAILABLE
}

async fn apply_request_timeout(
    State(budget): State<TimeoutBudget>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let limit = match is_streaming_write(&request) {
        true => budget.extended,
        false => budget.standard,
    };
    match tokio::time::timeout(limit, next.run(request)).await {
        Ok(response) => response,
        Err(_) => StatusCode::REQUEST_TIMEOUT.into_response(),
    }
}

fn is_streaming_write(request: &Request<Body>) -> bool {
    let path = request.uri().path();
    match *request.method() {
        Method::POST => path.ends_with("/git-receive-pack"),
        Method::PUT => path.contains("/info/lfs/objects/"),
        _ => false,
    }
}

fn rewrap_body(body: TimeoutBody<axum::body::Body>) -> axum::body::Body {
    axum::body::Body::new(body)
}

pub(crate) fn apply(router: Router, layers: GuardLayers) -> Router {
    let GuardLayers {
        governor,
        max_inflight,
        body_timeout,
        timeout,
    } = layers;
    let rate_limit: GovernorLayer<ProxyAwareIp, NoOpMiddleware, axum::body::Body> =
        GovernorLayer::new(governor);
    router
        .layer(
            ServiceBuilder::new()
                .layer(RequestBodyTimeoutLayer::new(body_timeout))
                .layer(MapRequestBodyLayer::new(rewrap_body)),
        )
        .layer(from_fn_with_state(timeout, apply_request_timeout))
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(shed_overloaded))
                .layer(LoadShedLayer::new())
                .layer(GlobalConcurrencyLimitLayer::new(max_inflight)),
        )
        .layer(rate_limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::{Body, Bytes};
    use axum::routing::{get, post};
    use futures::StreamExt;
    use http::StatusCode;
    use tower::ServiceExt;

    fn guards(
        rate: u32,
        burst: u32,
        inflight: u32,
        request_timeout_ms: u64,
        body_timeout_ms: u64,
        proxy_header: Option<&str>,
    ) -> EdgeGuards {
        guards_with_write(
            rate,
            burst,
            inflight,
            request_timeout_ms,
            body_timeout_ms,
            request_timeout_ms,
            proxy_header,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn guards_with_write(
        rate: u32,
        burst: u32,
        inflight: u32,
        request_timeout_ms: u64,
        body_timeout_ms: u64,
        write_request_timeout_ms: u64,
        proxy_header: Option<&str>,
    ) -> EdgeGuards {
        EdgeGuards::new(
            RequestsPerSecond::new(NonZeroU32::new(rate).unwrap()),
            BurstSize::new(NonZeroU32::new(burst).unwrap()),
            MaxInflightRequests::new(NonZeroU32::new(inflight).unwrap()),
            RequestTimeout::from_millis(NonZeroU64::new(request_timeout_ms).unwrap()),
            BodyInactivityTimeout::from_millis(NonZeroU64::new(body_timeout_ms).unwrap()),
            WriteRequestTimeout::from_millis(NonZeroU64::new(write_request_timeout_ms).unwrap()),
            proxy_header.map(|header| HeaderName::from_bytes(header.as_bytes()).unwrap()),
        )
    }

    fn guarded_router(router: Router, guards: EdgeGuards) -> Router {
        apply(router, guards.prepare(&CancellationToken::new()))
    }

    fn from_peer(request: Request<Body>, host: u8) -> Request<Body> {
        let mut request = request;
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, host], 47000))));
        request
    }

    fn get_request() -> Request<Body> {
        Request::get("/").body(Body::empty()).unwrap()
    }

    #[test]
    fn the_extractor_keys_on_the_trusted_proxy_header_when_configured() {
        let extractor = ProxyAwareIp {
            header: Some(HeaderName::from_static("x-forwarded-for")),
        };
        let request = Request::get("/")
            .header("x-forwarded-for", "203.0.113.7, 198.51.100.4")
            .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 5000))))
            .body(())
            .unwrap();
        assert_eq!(
            extractor.extract(&request).unwrap(),
            "198.51.100.4".parse::<IpAddr>().unwrap(),
            "the rightmost forwarded entry is the client the proxy appended"
        );
    }

    #[test]
    fn the_extractor_ignores_a_forgeable_header_when_no_proxy_is_trusted() {
        let extractor = ProxyAwareIp { header: None };
        let request = Request::get("/")
            .header("x-forwarded-for", "203.0.113.7")
            .extension(ConnectInfo(SocketAddr::from(([10, 0, 0, 9], 5000))))
            .body(())
            .unwrap();
        assert_eq!(
            extractor.extract(&request).unwrap(),
            "10.0.0.9".parse::<IpAddr>().unwrap(),
            "with no trusted proxy the socket peer wins over a client-forgeable header"
        );
    }

    #[test]
    fn the_extractor_falls_back_to_the_peer_when_the_trusted_header_is_absent() {
        let extractor = ProxyAwareIp {
            header: Some(HeaderName::from_static("x-forwarded-for")),
        };
        let request = Request::get("/")
            .extension(ConnectInfo(SocketAddr::from(([10, 0, 0, 9], 5000))))
            .body(())
            .unwrap();
        assert_eq!(
            extractor.extract(&request).unwrap(),
            "10.0.0.9".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn the_extractor_fails_when_no_peer_can_be_identified() {
        let extractor = ProxyAwareIp { header: None };
        let request = Request::get("/").body(()).unwrap();
        assert!(matches!(
            extractor.extract(&request),
            Err(GovernorError::UnableToExtractKey)
        ));
    }

    #[tokio::test]
    async fn a_well_behaved_request_passes_every_guard() {
        let app = guarded_router(
            Router::new().route("/", get(|| async { "ok" })),
            guards(50, 200, 1_024, 60_000, 30_000, None),
        );
        let status = app
            .oneshot(from_peer(get_request(), 1))
            .await
            .unwrap()
            .status();
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_burst_beyond_the_per_ip_limit_is_rejected_with_429() {
        let app = guarded_router(
            Router::new().route("/", get(|| async { "ok" })),
            guards(1, 2, 1_024, 60_000, 30_000, None),
        );
        let first = app
            .clone()
            .oneshot(from_peer(get_request(), 7))
            .await
            .unwrap()
            .status();
        let second = app
            .clone()
            .oneshot(from_peer(get_request(), 7))
            .await
            .unwrap()
            .status();
        let third = app
            .clone()
            .oneshot(from_peer(get_request(), 7))
            .await
            .unwrap()
            .status();
        let other_ip = app
            .clone()
            .oneshot(from_peer(get_request(), 8))
            .await
            .unwrap()
            .status();
        assert_eq!(first, StatusCode::OK);
        assert_eq!(second, StatusCode::OK);
        assert_eq!(
            third,
            StatusCode::TOO_MANY_REQUESTS,
            "a third request inside the window exhausts the burst for this IP"
        );
        assert_eq!(
            other_ip,
            StatusCode::OK,
            "a different IP keeps its own independent quota"
        );
    }

    #[tokio::test]
    async fn requests_beyond_the_inflight_limit_are_shed_with_503() {
        let app = guarded_router(
            Router::new().route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    "ok"
                }),
            ),
            guards(10_000, 10_000, 1, 60_000, 30_000, None),
        );
        let holder = {
            let app = app.clone();
            tokio::spawn(async move {
                let request = from_peer(Request::get("/slow").body(Body::empty()).unwrap(), 1);
                app.oneshot(request).await.unwrap().status()
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let shed = app
            .clone()
            .oneshot(from_peer(
                Request::get("/slow").body(Body::empty()).unwrap(),
                2,
            ))
            .await
            .unwrap()
            .status();
        assert_eq!(
            shed,
            StatusCode::SERVICE_UNAVAILABLE,
            "with the single inflight slot held, the next request sheds rather than queues"
        );
        assert_eq!(holder.await.unwrap(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_request_slower_than_the_timeout_is_cut_with_408() {
        let app = guarded_router(
            Router::new().route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    "ok"
                }),
            ),
            guards(10_000, 10_000, 1_024, 80, 30_000, None),
        );
        let status = app
            .oneshot(from_peer(
                Request::get("/slow").body(Body::empty()).unwrap(),
                1,
            ))
            .await
            .unwrap()
            .status();
        assert_eq!(status, StatusCode::REQUEST_TIMEOUT);
    }

    #[tokio::test]
    async fn a_streaming_write_runs_under_the_extended_budget_while_reads_keep_the_standard_one() {
        let app = guarded_router(
            Router::new()
                .route(
                    "/did/name/git-receive-pack",
                    post(|| async {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        "ok"
                    }),
                )
                .route(
                    "/did/name/git-upload-pack",
                    post(|| async {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        "ok"
                    }),
                ),
            guards_with_write(10_000, 10_000, 1_024, 80, 30_000, 5_000, None),
        );
        let push = app
            .clone()
            .oneshot(from_peer(
                Request::post("/did/name/git-receive-pack")
                    .body(Body::empty())
                    .unwrap(),
                1,
            ))
            .await
            .unwrap()
            .status();
        assert_eq!(
            push,
            StatusCode::OK,
            "a push slower than the standard timeout survives on the extended write budget"
        );
        let fetch = app
            .oneshot(from_peer(
                Request::post("/did/name/git-upload-pack")
                    .body(Body::empty())
                    .unwrap(),
                1,
            ))
            .await
            .unwrap()
            .status();
        assert_eq!(
            fetch,
            StatusCode::REQUEST_TIMEOUT,
            "a non-write request past the standard timeout is still cut"
        );
    }

    #[tokio::test]
    async fn a_stalled_request_body_is_cut_and_never_hangs() {
        let app = guarded_router(
            Router::new().route("/upload", post(|_body: Bytes| async { "ok" })),
            guards(10_000, 10_000, 1_024, 60_000, 80, None),
        );
        let body = Body::from_stream(
            futures::stream::once(async {
                Ok::<_, std::io::Error>(Bytes::from_static(b"partial"))
            })
            .chain(futures::stream::pending::<Result<Bytes, std::io::Error>>()),
        );
        let request = from_peer(Request::post("/upload").body(body).unwrap(), 1);
        let status = tokio::time::timeout(Duration::from_secs(5), app.oneshot(request))
            .await
            .expect("the body inactivity timeout must cut a stalled upload instead of hanging")
            .unwrap()
            .status();
        assert_ne!(
            status,
            StatusCode::OK,
            "a body that stalls past the inactivity timeout is never accepted"
        );
    }
}
