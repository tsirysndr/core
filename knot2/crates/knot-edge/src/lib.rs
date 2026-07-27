mod acme;
mod altsvc;
mod compression;
mod limits;
mod peer;
mod protocol;
mod quic;
mod robustness;
mod tcp;
mod tls;
mod zerortt;

use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use axum::Router;
use axum::middleware::from_fn;
use rustls::server::ResolvesServerCert;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

pub use acme::{AcmeCacheDir, AcmeContact, AcmeContactError, AcmeError, AcmeParams};
pub use limits::{
    ConnectionBudget, HeaderTimeout, IdleTimeout, ListenLimits, MaxConcurrentStreams,
};
pub use peer::SocketPeer;
pub use protocol::NegotiatedProtocol;
pub use quic::EndpointError;
pub use robustness::{
    BodyInactivityTimeout, BurstSize, EdgeGuards, MaxInflightRequests, RequestTimeout,
    RequestsPerSecond, WriteRequestTimeout,
};
pub use tls::{ReloadableCertResolver, SpkiPin, TlsError, load_certified_key};
pub use zerortt::{EarlyData, RequiresFullHandshake, ZeroRttRoutes, ZeroRttSafe};

pub mod fuzz {
    pub fn spki_of_certificate(data: &[u8]) {
        crate::tls::fuzz_of_certificate(data);
    }

    pub fn spki_pin(data: &[u8]) {
        let _ = crate::SpkiPin::from_base64(&String::from_utf8_lossy(data));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertChainPath(PathBuf);

impl CertChainPath {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateKeyPath(PathBuf);

impl PrivateKeyPath {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientCaPath(PathBuf);

impl ClientCaPath {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicBind(SocketAddr);

impl PublicBind {
    pub const fn new(addr: SocketAddr) -> Self {
        Self(addr)
    }

    pub const fn get(self) -> SocketAddr {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InternalBind(SocketAddr);

impl InternalBind {
    pub const fn new(addr: SocketAddr) -> Self {
        Self(addr)
    }

    pub const fn get(self) -> SocketAddr {
        self.0
    }
}

pub struct StaticCertPaths {
    pub cert_path: CertChainPath,
    pub key_path: PrivateKeyPath,
}

pub enum CertSource {
    Static(StaticCertPaths),
    Acme(AcmeParams),
}

pub struct InternalTls {
    pub addr: InternalBind,
    pub client_ca_path: ClientCaPath,
    pub spki_pin: SpkiPin,
}

pub struct TlsSetup {
    pub source: CertSource,
    pub http3: bool,
    pub internal: Option<InternalTls>,
}

pub struct EdgeConfig {
    pub http_addr: PublicBind,
    pub limits: ListenLimits,
    pub guards: EdgeGuards,
    pub tls: Option<TlsSetup>,
}

#[derive(Debug, thiserror::Error)]
pub enum EdgeError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Tls(#[from] TlsError),
    #[error(transparent)]
    Acme(#[from] AcmeError),
    #[error(transparent)]
    Endpoint(#[from] EndpointError),
}

type Served = Pin<Box<dyn Future<Output = Result<(), EdgeError>> + Send>>;

fn base_router(app: RequiresFullHandshake, early_data_safe: ZeroRttRoutes) -> Router {
    early_data_safe
        .into_router()
        .merge(app.into_router())
        .layer(compression::layer())
        .layer(from_fn(zerortt::tag_from_header))
}

fn finish(router: Router) -> Router {
    altsvc::with_host_from_authority(router.layer(from_fn(protocol::tag)))
}

pub async fn serve(
    config: EdgeConfig,
    app: RequiresFullHandshake,
    early_data_safe: ZeroRttRoutes,
    shutdown: CancellationToken,
) -> Result<(), EdgeError> {
    let EdgeConfig {
        http_addr,
        limits,
        guards,
        tls,
    } = config;
    let layers = guards.prepare(&shutdown);
    let early_data = early_data_safe.early_data_policy();
    let wants_internal = tls
        .as_ref()
        .and_then(|setup| setup.internal.as_ref())
        .is_some();
    let base = base_router(app, early_data_safe);
    let internal_router = wants_internal.then(|| finish(base.clone()));
    let router = finish(robustness::apply(base, layers));
    let listener = TcpListener::bind(http_addr.get()).await?;

    let Some(setup) = tls else {
        return Ok(tcp::serve_plaintext(listener, router, limits, shutdown).await?);
    };

    let (resolver, acme): (Arc<dyn ResolvesServerCert>, bool) = match setup.source {
        CertSource::Static(paths) => {
            let certified = tls::load_certified_key(&paths)?;
            let reloadable = Arc::new(ReloadableCertResolver::new(certified));
            tls::spawn_cert_reload(Arc::clone(&reloadable), paths, shutdown.clone());
            (reloadable, false)
        }
        CertSource::Acme(params) => (acme::start(params, shutdown.clone())?, true),
    };

    let extra_alpn: &[&[u8]] = if acme { &[tls::ACME_TLS_ALPN] } else { &[] };
    let tcp_config = Arc::new(tls::build_tls_server_config(
        Arc::clone(&resolver),
        extra_alpn,
    )?);
    let port = http_addr.get().port();

    let mut servers: Vec<Served> = Vec::new();

    servers.push({
        let app = match setup.http3 {
            true => altsvc::with_alt_svc(router.clone(), altsvc::Port::new(port)),
            false => router.clone(),
        };
        let shutdown = shutdown.clone();
        Box::pin(async move {
            let result = tcp::serve_tls(listener, app, tcp_config, limits, shutdown.clone()).await;
            shutdown.cancel();
            Ok(result?)
        })
    });

    if setup.http3 {
        let endpoint =
            quic::build_endpoint(http_addr.get(), Arc::clone(&resolver), limits, early_data)?;
        let app = router.clone();
        let shutdown = shutdown.clone();
        servers.push(Box::pin(async move {
            quic::serve_http3(endpoint, app, limits, shutdown.clone()).await;
            shutdown.cancel();
            Ok(())
        }));
    }

    if let Some(internal) = setup.internal {
        let internal_listener = TcpListener::bind(internal.addr.get()).await?;
        let mtls_config = Arc::new(tls::build_mtls_server_config(
            Arc::clone(&resolver),
            &internal.client_ca_path,
            internal.spki_pin,
        )?);
        let app = internal_router
            .expect("an internal router is built whenever an internal bind is configured");
        let shutdown = shutdown.clone();
        servers.push(Box::pin(async move {
            let result = tcp::serve_tls(
                internal_listener,
                app,
                mtls_config,
                limits,
                shutdown.clone(),
            )
            .await;
            shutdown.cancel();
            Ok(result?)
        }));
    }

    futures::future::join_all(servers)
        .await
        .into_iter()
        .collect::<Result<Vec<()>, EdgeError>>()
        .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::routing::post;
    use http::{Request, StatusCode};
    use tower::ServiceExt;

    use std::num::{NonZeroU32, NonZeroU64};

    use zerortt::ZeroRttSafe;

    fn test_layers() -> robustness::GuardLayers {
        EdgeGuards::new(
            RequestsPerSecond::new(NonZeroU32::new(10_000).unwrap()),
            BurstSize::new(NonZeroU32::new(10_000).unwrap()),
            MaxInflightRequests::new(NonZeroU32::new(1_024).unwrap()),
            RequestTimeout::from_millis(NonZeroU64::new(30_000).unwrap()),
            BodyInactivityTimeout::from_millis(NonZeroU64::new(30_000).unwrap()),
            WriteRequestTimeout::from_millis(NonZeroU64::new(1_800_000).unwrap()),
            None,
        )
        .prepare(&CancellationToken::new())
    }

    fn wired() -> Router {
        let safe =
            ZeroRttRoutes::new().get("/info/refs", ZeroRttSafe::new(|| async { "advertisement" }));
        let full = RequiresFullHandshake::new(
            Router::new().route("/git-upload-pack", post(|| async { "pack" })),
        );
        finish(robustness::apply(base_router(full, safe), test_layers()))
    }

    fn tight_layers() -> robustness::GuardLayers {
        EdgeGuards::new(
            RequestsPerSecond::new(NonZeroU32::new(1).unwrap()),
            BurstSize::new(NonZeroU32::new(2).unwrap()),
            MaxInflightRequests::new(NonZeroU32::new(1_024).unwrap()),
            RequestTimeout::from_millis(NonZeroU64::new(30_000).unwrap()),
            BodyInactivityTimeout::from_millis(NonZeroU64::new(30_000).unwrap()),
            WriteRequestTimeout::from_millis(NonZeroU64::new(1_800_000).unwrap()),
            None,
        )
        .prepare(&CancellationToken::new())
    }

    async fn status_of(mut request: Request<Body>) -> StatusCode {
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 41001))));
        wired().oneshot(request).await.unwrap().status()
    }

    #[tokio::test]
    async fn a_write_in_early_data_is_refused_with_425() {
        let request = Request::post("/git-upload-pack")
            .header("early-data", "1")
            .body(Body::empty())
            .unwrap();
        assert_eq!(status_of(request).await, StatusCode::TOO_EARLY);
    }

    #[tokio::test]
    async fn a_write_after_the_handshake_is_served() {
        let request = Request::post("/git-upload-pack")
            .body(Body::empty())
            .unwrap();
        assert_eq!(status_of(request).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn the_advertisement_is_served_even_in_early_data() {
        let request = Request::get("/info/refs")
            .header("early-data", "1")
            .body(Body::empty())
            .unwrap();
        assert_eq!(status_of(request).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn the_internal_admin_router_shares_no_rate_limit_budget_with_the_data_plane() {
        let safe = ZeroRttRoutes::new().get("/info/refs", ZeroRttSafe::new(|| async { "ok" }));
        let full = RequiresFullHandshake::new(
            Router::new().route("/git-upload-pack", post(|| async { "pack" })),
        );
        let base = base_router(full, safe);
        let public = finish(robustness::apply(base.clone(), tight_layers()));
        let internal = finish(base);

        let request = || {
            let mut request = Request::get("/info/refs").body(Body::empty()).unwrap();
            request
                .extensions_mut()
                .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 41001))));
            request
        };

        let p1 = public.clone().oneshot(request()).await.unwrap().status();
        let p2 = public.clone().oneshot(request()).await.unwrap().status();
        let p3 = public.clone().oneshot(request()).await.unwrap().status();
        assert_eq!([p1, p2], [StatusCode::OK, StatusCode::OK]);
        assert_eq!(
            p3,
            StatusCode::TOO_MANY_REQUESTS,
            "the public edge still enforces the per-IP burst"
        );

        let i1 = internal.clone().oneshot(request()).await.unwrap().status();
        let i2 = internal.clone().oneshot(request()).await.unwrap().status();
        let i3 = internal.clone().oneshot(request()).await.unwrap().status();
        let i4 = internal.clone().oneshot(request()).await.unwrap().status();
        assert_eq!(
            [i1, i2, i3, i4],
            [StatusCode::OK; 4],
            "the internal admin bind is unguarded, so a public flood never sheds admin requests"
        );
    }
}
