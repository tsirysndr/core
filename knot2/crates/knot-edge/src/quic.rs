use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use bytes::{Buf, Bytes};
use futures::{FutureExt, StreamExt};
use http::{Request, Response, Version};
use quinn::{Endpoint, Incoming};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tower::ServiceExt;

use rustls::server::ResolvesServerCert;

use crate::limits::ListenLimits;
use crate::tls::{self, TlsError};
use crate::zerortt::{EarlyData, EarlyDataPolicy};

fn early_data(confirmed: bool) -> EarlyData {
    match confirmed {
        true => EarlyData::No,
        false => EarlyData::Yes,
    }
}

const LISTENER_DRAIN_GRACE: Duration = Duration::from_secs(30);
const ENDPOINT_DRAIN_GRACE: Duration = Duration::from_secs(5);
const CONNECTION_DRAIN_GRACE: Duration = Duration::from_secs(10);

pub fn build_endpoint(
    addr: SocketAddr,
    resolver: Arc<dyn ResolvesServerCert>,
    limits: ListenLimits,
    early_data: EarlyDataPolicy,
) -> Result<Endpoint, EndpointError> {
    let server_config = tls::build_quic_server_config(resolver, limits, early_data)?;
    let endpoint = Endpoint::server(server_config, addr)?;
    Ok(endpoint)
}

#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    #[error(transparent)]
    Tls(#[from] TlsError),
    #[error("binding quic socket: {0}")]
    Bind(#[from] std::io::Error),
}

pub async fn serve_http3(
    endpoint: Endpoint,
    app: Router,
    limits: ListenLimits,
    shutdown: CancellationToken,
) {
    let tracker = TaskTracker::new();
    let connections = Arc::new(Semaphore::new(limits.max_connections()));
    loop {
        let incoming = tokio::select! {
            () = shutdown.cancelled() => break,
            incoming = endpoint.accept() => incoming,
        };
        let Some(incoming) = incoming else { break };
        let Ok(permit) = Arc::clone(&connections).try_acquire_owned() else {
            incoming.refuse();
            continue;
        };
        let app = app.clone();
        let conn_shutdown = shutdown.clone();
        let conn_tracker = tracker.clone();
        tracker.spawn(async move {
            let _permit = permit;
            if let Err(error) = serve_connection(incoming, app, conn_shutdown, conn_tracker).await {
                tracing::debug!("h3 connection ended: {error}");
            }
        });
    }
    tracker.close();
    let _ = tokio::time::timeout(LISTENER_DRAIN_GRACE, tracker.wait()).await;
    endpoint.close(0u32.into(), b"shutdown");
    let _ = tokio::time::timeout(ENDPOINT_DRAIN_GRACE, endpoint.wait_idle()).await;
}

async fn serve_connection(
    incoming: Incoming,
    app: Router,
    shutdown: CancellationToken,
    tracker: TaskTracker,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (conn, confirmation, mut confirmed) = match incoming.accept()?.into_0rtt() {
        Ok((conn, accepted)) => (conn, accepted.map(|_| ()).left_future(), false),
        Err(connecting) => (
            connecting.await?,
            std::future::pending::<()>().right_future(),
            true,
        ),
    };
    tokio::pin!(confirmation);
    let remote = conn.remote_address();
    let quic = conn.clone();
    tracing::trace!("h3 connection from {remote} accepted");
    let mut h3_conn =
        h3::server::Connection::<_, Bytes>::new(h3_quinn::Connection::new(conn)).await?;
    tracing::trace!("h3 connection from {remote} established");

    let drain_deadline = tokio::time::sleep(CONNECTION_DRAIN_GRACE);
    tokio::pin!(drain_deadline);
    let mut draining = false;
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled(), if !draining => {
                draining = true;
                drain_deadline
                    .as_mut()
                    .reset(tokio::time::Instant::now() + CONNECTION_DRAIN_GRACE);
                let _ = h3_conn.shutdown(0).await;
            }
            () = &mut drain_deadline, if draining => {
                tracing::debug!("h3 connection from {remote} drain timed out, closing");
                quic.close(0u32.into(), b"drain timeout");
                break;
            }
            resolved = h3_conn.accept() => match resolved {
                Ok(Some(resolver)) => {
                    let app = app.clone();
                    let early = early_data(confirmed);
                    tracker.spawn(async move {
                        if let Err(error) = serve_request(resolver, app, remote, early).await {
                            tracing::debug!("h3 request from {remote} failed: {error}");
                        }
                    });
                }
                Ok(None) => {
                    tracing::debug!("h3 connection from {remote} closed by the client");
                    break;
                }
                Err(error) => {
                    tracing::debug!("h3 accept from {remote} error: {error}");
                    break;
                }
            },
            () = &mut confirmation, if !confirmed => {
                confirmed = true;
            }
        }
    }
    Ok(())
}

async fn serve_request(
    resolver: h3::server::RequestResolver<h3_quinn::Connection, Bytes>,
    app: Router,
    remote: SocketAddr,
    early: EarlyData,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (request, stream) = resolver.resolve_request().await?;
    let (mut send, recv) = stream.split();

    let (mut parts, ()) = request.into_parts();
    parts.version = Version::HTTP_3;
    parts.extensions.insert(ConnectInfo(remote));
    parts.extensions.insert(early);
    let request = Request::from_parts(parts, request_body(recv));

    let response = match app.oneshot(request).await {
        Ok(response) => response,
        Err(infallible) => match infallible {},
    };

    let (parts, body) = response.into_parts();
    send.send_response(Response::from_parts(parts, ())).await?;

    let mut data = body.into_data_stream();
    while let Some(chunk) = data.next().await {
        match chunk {
            Ok(bytes) if bytes.has_remaining() => send.send_data(bytes).await?,
            Ok(_) => {}
            Err(error) => {
                tracing::debug!("h3 response body to {remote} errored: {error}");
                send.stop_stream(h3::error::Code::H3_INTERNAL_ERROR);
                return Ok(());
            }
        }
    }
    send.finish().await?;
    Ok(())
}

struct RecvGuard {
    stream: h3::server::RequestStream<h3_quinn::RecvStream, Bytes>,
    ended: bool,
}

impl Drop for RecvGuard {
    fn drop(&mut self) {
        if !self.ended {
            self.stream.stop_sending(h3::error::Code::H3_NO_ERROR);
        }
    }
}

fn request_body(recv: h3::server::RequestStream<h3_quinn::RecvStream, Bytes>) -> Body {
    let guard = RecvGuard {
        stream: recv,
        ended: false,
    };
    let stream = futures::stream::unfold(Some(guard), |state| async move {
        let mut guard = state?;
        match guard.stream.recv_data().await {
            Ok(Some(mut buf)) => {
                let bytes = buf.copy_to_bytes(buf.remaining());
                Some((Ok::<Bytes, std::io::Error>(bytes), Some(guard)))
            }
            Ok(None) => {
                guard.ended = true;
                None
            }
            Err(error) => {
                guard.ended = true;
                Some((Err(std::io::Error::other(error.to_string())), None))
            }
        }
    });
    Body::from_stream(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::routing::get;
    use rustls::crypto::aws_lc_rs;

    use crate::tls;

    #[test]
    fn an_unconfirmed_handshake_is_early_data_and_a_confirmed_one_is_not() {
        assert_eq!(
            early_data(false),
            EarlyData::Yes,
            "data before handshake confirmation is early data, fail closed"
        );
        assert_eq!(early_data(true), EarlyData::No);
    }

    fn client_endpoint() -> Endpoint {
        client_endpoint_with_provider(aws_lc_rs::default_provider())
    }

    fn client_endpoint_with_provider(provider: rustls::crypto::CryptoProvider) -> Endpoint {
        let mut crypto = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(tls::test_support::AcceptAnyServerCert))
            .with_no_client_auth();
        crypto.alpn_protocols = vec![b"h3".to_vec()];
        let quic = quinn::crypto::rustls::QuicClientConfig::try_from(crypto).unwrap();
        let mut endpoint = Endpoint::client("[::1]:0".parse().unwrap()).unwrap();
        endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic)));
        endpoint
    }

    fn spawn_h3_server(
        app: Router,
        limits: ListenLimits,
        early_data: EarlyDataPolicy,
    ) -> (SocketAddr, CancellationToken) {
        let endpoint = build_endpoint(
            "[::1]:0".parse().unwrap(),
            tls::test_support::resolver(),
            limits,
            early_data,
        )
        .unwrap();
        let addr = endpoint.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        tokio::spawn(serve_http3(
            endpoint,
            crate::altsvc::with_host_from_authority(app),
            limits,
            shutdown.clone(),
        ));
        (addr, shutdown)
    }

    async fn h3_client_connect(
        client: &Endpoint,
        addr: SocketAddr,
    ) -> h3::client::SendRequest<h3_quinn::OpenStreams, Bytes> {
        let conn = client.connect(addr, "localhost").unwrap().await.unwrap();
        let (mut driver, send_request) = h3::client::new(h3_quinn::Connection::new(conn))
            .await
            .unwrap();
        tokio::spawn(async move { std::future::poll_fn(|cx| driver.poll_close(cx)).await });
        send_request
    }

    async fn h3_request(
        send_request: &mut h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>,
        path: &str,
    ) -> (http::StatusCode, Vec<u8>) {
        let request = http::Request::get(format!("https://localhost{path}"))
            .body(())
            .unwrap();
        let mut stream = send_request.send_request(request).await.unwrap();
        stream.finish().await.unwrap();
        let status = stream.recv_response().await.unwrap().status();
        let chunks: Vec<Bytes> = futures::stream::unfold(stream, |mut stream| async move {
            stream
                .recv_data()
                .await
                .unwrap()
                .map(|mut buf| (buf.copy_to_bytes(buf.remaining()), stream))
        })
        .collect()
        .await;
        let body = chunks
            .iter()
            .flat_map(|chunk| chunk.iter().copied())
            .collect();
        (status, body)
    }

    #[tokio::test]
    async fn an_h3_get_roundtrips_through_the_router() {
        let app = Router::new().route(
            "/",
            get(|ConnectInfo(peer): ConnectInfo<SocketAddr>| async move { peer.to_string() }),
        );
        let (addr, shutdown) =
            spawn_h3_server(app, tls::test_support::limits(), EarlyDataPolicy::Disabled);

        let client = client_endpoint();
        let client_port = client.local_addr().unwrap().port();
        let mut send_request = h3_client_connect(&client, addr).await;
        let (status, body) = h3_request(&mut send_request, "/").await;
        assert_eq!(status, 200);
        let reported: SocketAddr = String::from_utf8(body).unwrap().parse().unwrap();
        assert_eq!(
            reported.port(),
            client_port,
            "the h3 handler must see the QUIC remote address via ConnectInfo"
        );
        shutdown.cancel();
    }

    #[tokio::test]
    async fn an_h3_request_is_tagged_with_the_h3_protocol() {
        use axum::middleware::from_fn;

        let app = Router::new()
            .route(
                "/proto",
                get(|req: axum::extract::Request| async move {
                    req.extensions()
                        .get::<crate::protocol::NegotiatedProtocol>()
                        .map(|protocol| protocol.as_str())
                        .unwrap_or("missing")
                        .to_string()
                }),
            )
            .layer(from_fn(crate::protocol::tag));
        let (addr, shutdown) =
            spawn_h3_server(app, tls::test_support::limits(), EarlyDataPolicy::Disabled);

        let client = client_endpoint();
        let mut send_request = h3_client_connect(&client, addr).await;
        let (status, body) = h3_request(&mut send_request, "/proto").await;
        assert_eq!(status, 200);
        assert_eq!(
            String::from_utf8(body).unwrap(),
            "h3",
            "a request served over QUIC must be tagged as the h3 negotiated protocol"
        );
        shutdown.cancel();
    }

    #[tokio::test]
    async fn an_early_data_enabled_endpoint_still_serves_through_the_zero_rtt_path() {
        let app = Router::new().route("/", get(|| async { "ok" }));
        let (addr, shutdown) =
            spawn_h3_server(app, tls::test_support::limits(), EarlyDataPolicy::Enabled);

        let client = client_endpoint();
        let mut send_request = h3_client_connect(&client, addr).await;
        let (status, _) = h3_request(&mut send_request, "/").await;
        assert_eq!(
            status, 200,
            "a server with early data enabled must serve requests through the 0-RTT acceptance path"
        );
        shutdown.cancel();
    }

    #[tokio::test]
    async fn a_classical_only_h3_client_completes_over_x25519() {
        let app = Router::new().route("/", get(|| async { "ok" }));
        let (addr, shutdown) =
            spawn_h3_server(app, tls::test_support::limits(), EarlyDataPolicy::Disabled);

        let client = client_endpoint_with_provider(tls::test_support::classical_only_provider());
        let mut send_request = h3_client_connect(&client, addr).await;
        let (status, _) = h3_request(&mut send_request, "/").await;
        assert_eq!(
            status, 200,
            "a QUIC client without ML-KEM must still complete the h3 handshake over classical X25519"
        );
        shutdown.cancel();
    }

    #[tokio::test]
    async fn an_in_flight_h3_request_finishes_during_drain() {
        let app = Router::new().route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                "drained-clean"
            }),
        );
        let (addr, shutdown) =
            spawn_h3_server(app, tls::test_support::limits(), EarlyDataPolicy::Disabled);

        let client = client_endpoint();
        let mut send_request = h3_client_connect(&client, addr).await;
        let ((status, body), ()) = tokio::join!(h3_request(&mut send_request, "/slow"), async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            shutdown.cancel();
        });
        assert_eq!(
            status, 200,
            "an in-flight h3 request must complete through the graceful drain"
        );
        assert_eq!(body, b"drained-clean");
        shutdown.cancel();
    }

    #[tokio::test]
    async fn an_h3_connection_survives_repeated_client_path_migration() {
        let app = Router::new().route("/echo", get(|| async { "migrated-clean" }));
        let (addr, shutdown) =
            spawn_h3_server(app, tls::test_support::limits(), EarlyDataPolicy::Disabled);

        let client = client_endpoint();
        let send_request = h3_client_connect(&client, addr).await;
        futures::stream::iter(0u8..3)
            .fold(
                (client, send_request),
                |(client, mut send_request), round| async move {
                    client
                        .rebind(std::net::UdpSocket::bind("[::1]:0").unwrap())
                        .unwrap();
                    let (status, body) = h3_request(&mut send_request, "/echo").await;
                    assert_eq!(
                        status, 200,
                        "round {round}: a request after path migration must still be served"
                    );
                    assert_eq!(
                        body, b"migrated-clean",
                        "round {round}: the migrated connection must deliver the response uncorrupted"
                    );
                    (client, send_request)
                },
            )
            .await;

        shutdown.cancel();
    }

    #[tokio::test]
    async fn a_slow_h3_response_survives_the_idle_timeout() {
        use std::num::{NonZeroU32, NonZeroU64};

        let limits = ListenLimits::new(
            crate::limits::HeaderTimeout::from_millis(NonZeroU64::new(1_000).unwrap()),
            crate::limits::IdleTimeout::from_millis(NonZeroU64::new(2_000).unwrap()),
            NonZeroU32::new(64).unwrap(),
        );
        let app = Router::new().route(
            "/",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(3)).await;
                "ok"
            }),
        );
        let (addr, shutdown) = spawn_h3_server(app, limits, EarlyDataPolicy::Disabled);

        let client = client_endpoint();
        let mut send_request = h3_client_connect(&client, addr).await;
        let (status, body) = h3_request(&mut send_request, "/").await;
        assert_eq!(
            status, 200,
            "keep-alive must hold the connection through a response slower than the idle timeout"
        );
        assert_eq!(body, b"ok");
        shutdown.cancel();
    }
}
