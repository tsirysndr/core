use std::net::{IpAddr, SocketAddr};
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::error_handling::HandleErrorLayer;
use axum::extract::{ConnectInfo, State};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use governor::middleware::NoOpMiddleware;
use http::{Method, Request, StatusCode};
use knot_types::ProxyTrust;
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
    proxy_trust: ProxyTrust,
}

impl EdgeGuards {
    pub fn new(
        rate: RequestsPerSecond,
        burst: BurstSize,
        max_inflight: MaxInflightRequests,
        request_timeout: RequestTimeout,
        body_timeout: BodyInactivityTimeout,
        write_request_timeout: WriteRequestTimeout,
        proxy_trust: ProxyTrust,
    ) -> Self {
        Self {
            rate,
            burst,
            max_inflight,
            request_timeout,
            body_timeout,
            write_request_timeout,
            proxy_trust,
        }
    }

    pub(crate) fn prepare(self, shutdown: &CancellationToken) -> GuardLayers {
        let governor = build_governor(self.rate, self.burst, self.proxy_trust);
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

#[derive(Clone, Default)]
struct IgnoredHeaderNotice(Arc<OnceLock<IpAddr>>);

impl IgnoredHeaderNotice {
    fn report(&self, peer: IpAddr) {
        if self.0.set(peer).is_ok() {
            tracing::warn!(
                %peer,
                "a peer outside xrpc.trusted_proxies sent xrpc.trusted_proxy_header, so the knot ignored the header and rate-limits that peer by the address it connected from. Add this address to xrpc.trusted_proxies if it is the reverse proxy, since a proxy reaches the knot over one address family and listing the other one silently loses the header. This warning reports the first such peer only."
            );
        }
    }
}

#[derive(Clone)]
struct ProxyAwareIp {
    trust: ProxyTrust,
    ignored_header: IgnoredHeaderNotice,
}

impl KeyExtractor for ProxyAwareIp {
    type Key = IpAddr;

    fn extract<T>(&self, request: &Request<T>) -> Result<Self::Key, GovernorError> {
        let socket = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|info| info.0.ip());
        let key = self.trust.peer_key(request.headers(), socket);
        if let Some(peer) = key.ignored_header() {
            self.ignored_header.report(peer);
        }
        key.address().ok_or(GovernorError::UnableToExtractKey)
    }
}

fn build_governor(
    rate: RequestsPerSecond,
    burst: BurstSize,
    proxy_trust: ProxyTrust,
) -> Arc<GuardGovernor> {
    let mut builder = GovernorConfigBuilder::default();
    builder.period(rate.period()).burst_size(burst.0.get());
    let config = builder
        .key_extractor(ProxyAwareIp {
            trust: proxy_trust,
            ignored_header: IgnoredHeaderNotice::default(),
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
        proxy_trust: ProxyTrust,
    ) -> EdgeGuards {
        guards_with_write(
            rate,
            burst,
            inflight,
            request_timeout_ms,
            body_timeout_ms,
            request_timeout_ms,
            proxy_trust,
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
        proxy_trust: ProxyTrust,
    ) -> EdgeGuards {
        EdgeGuards::new(
            RequestsPerSecond::new(NonZeroU32::new(rate).unwrap()),
            BurstSize::new(NonZeroU32::new(burst).unwrap()),
            MaxInflightRequests::new(NonZeroU32::new(inflight).unwrap()),
            RequestTimeout::from_millis(NonZeroU64::new(request_timeout_ms).unwrap()),
            BodyInactivityTimeout::from_millis(NonZeroU64::new(body_timeout_ms).unwrap()),
            WriteRequestTimeout::from_millis(NonZeroU64::new(write_request_timeout_ms).unwrap()),
            proxy_trust,
        )
    }

    fn forwarded_for() -> http::HeaderName {
        http::HeaderName::from_static("x-forwarded-for")
    }

    fn trusting_any_peer() -> ProxyTrust {
        ProxyTrust::new(Some(forwarded_for()), knot_types::TrustedProxies::default())
    }

    fn trusting_loopback() -> ProxyTrust {
        ProxyTrust::new(
            Some(forwarded_for()),
            knot_types::TrustedProxies::new(["127.0.0.1".parse::<IpAddr>().unwrap()]),
        )
    }

    fn extractor(trust: ProxyTrust) -> ProxyAwareIp {
        ProxyAwareIp {
            trust,
            ignored_header: IgnoredHeaderNotice::default(),
        }
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
        let extractor = extractor(trusting_any_peer());
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
        let extractor = extractor(ProxyTrust::default());
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
    fn the_extractor_keys_an_unlisted_peer_on_its_socket_however_it_fills_the_header() {
        let extractor = extractor(trusting_loopback());
        let forged = |host| {
            Request::get("/")
                .header("x-forwarded-for", "198.51.100.4")
                .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, host], 5000))))
                .body(())
                .unwrap()
        };
        assert_eq!(
            extractor.extract(&forged(1)).unwrap(),
            "198.51.100.4".parse::<IpAddr>().unwrap(),
            "the listed proxy relayed this one, so the extractor keys on the header address"
        );
        assert_eq!(
            extractor.extract(&forged(9)).unwrap(),
            "127.0.0.9".parse::<IpAddr>().unwrap(),
            "an unlisted peer picked its own token bucket by forging the header"
        );
    }

    #[test]
    fn the_extractor_falls_back_to_the_peer_when_the_trusted_header_is_absent() {
        let extractor = extractor(trusting_any_peer());
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
        let extractor = extractor(ProxyTrust::default());
        let request = Request::get("/").body(()).unwrap();
        assert!(matches!(
            extractor.extract(&request),
            Err(GovernorError::UnableToExtractKey)
        ));
    }

    #[test]
    fn a_missing_connect_info_fails_closed_rather_than_taking_the_header_on_trust() {
        let listed = extractor(trusting_loopback());
        let headed = || {
            Request::get("/")
                .header("x-forwarded-for", "198.51.100.4")
                .body(())
                .unwrap()
        };
        assert!(
            matches!(
                listed.extract(&headed()),
                Err(GovernorError::UnableToExtractKey)
            ),
            "with no socket to match against the allowlist the extractor identifies no client"
        );
        assert_eq!(
            extractor(trusting_any_peer()).extract(&headed()).unwrap(),
            "198.51.100.4".parse::<IpAddr>().unwrap(),
            "an operator who lists no proxy already told the knot to take the header from anyone"
        );
    }

    #[test]
    fn the_first_unlisted_peer_sending_the_header_is_reported_once() {
        let extractor = extractor(trusting_loopback());
        let forged = |host| {
            Request::get("/")
                .header("x-forwarded-for", "198.51.100.4")
                .extension(ConnectInfo(SocketAddr::from(([203, 0, 113, host], 5000))))
                .body(())
                .unwrap()
        };
        extractor.extract(&forged(7)).unwrap();
        extractor.extract(&forged(9)).unwrap();
        assert_eq!(
            extractor.ignored_header.0.get(),
            Some(&"203.0.113.7".parse::<IpAddr>().unwrap()),
            "a wrong-family allowlist sends every request down this path, so only the first peer is reported"
        );
    }

    #[test]
    fn a_listed_proxy_and_a_headerless_request_report_nothing() {
        let listed = extractor(trusting_loopback());
        listed
            .extract(
                &Request::get("/")
                    .header("x-forwarded-for", "198.51.100.4")
                    .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 5000))))
                    .body(())
                    .unwrap(),
            )
            .unwrap();
        listed
            .extract(
                &Request::get("/")
                    .extension(ConnectInfo(SocketAddr::from(([203, 0, 113, 7], 5000))))
                    .body(())
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(
            listed.ignored_header.0.get(),
            None,
            "neither a relayed request or a request without the header says anything about the allowlist"
        );
    }

    #[tokio::test]
    async fn a_well_behaved_request_passes_every_guard() {
        let app = guarded_router(
            Router::new().route("/", get(|| async { "ok" })),
            guards(50, 200, 1_024, 60_000, 30_000, ProxyTrust::default()),
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
            guards(1, 2, 1_024, 60_000, 30_000, ProxyTrust::default()),
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
            guards(10_000, 10_000, 1, 60_000, 30_000, ProxyTrust::default()),
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
            guards(10_000, 10_000, 1_024, 80, 30_000, ProxyTrust::default()),
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
            guards_with_write(
                10_000,
                10_000,
                1_024,
                80,
                30_000,
                5_000,
                ProxyTrust::default(),
            ),
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
            guards(10_000, 10_000, 1_024, 60_000, 80, ProxyTrust::default()),
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
