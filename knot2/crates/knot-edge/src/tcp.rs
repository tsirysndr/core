use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use rustls::ServerConfig;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tower::{Service, ServiceExt};

use crate::limits::ListenLimits;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECTION_DRAIN_GRACE: Duration = Duration::from_secs(10);
const LISTENER_DRAIN_GRACE: Duration = Duration::from_secs(30);
const ACCEPT_BACKOFF: Duration = Duration::from_millis(250);

pub async fn serve_plaintext(
    listener: TcpListener,
    router: Router,
    limits: ListenLimits,
    shutdown: CancellationToken,
) -> std::io::Result<()> {
    run_listener(listener, router, limits, shutdown, |stream| async move {
        Some(stream)
    })
    .await
}

pub async fn serve_tls(
    listener: TcpListener,
    router: Router,
    server_config: Arc<ServerConfig>,
    limits: ListenLimits,
    shutdown: CancellationToken,
) -> std::io::Result<()> {
    let acceptor = TlsAcceptor::from(server_config);
    run_listener(listener, router, limits, shutdown, move |stream| {
        let acceptor = acceptor.clone();
        async move {
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                Ok(Ok(tls_stream)) => Some(tls_stream),
                Ok(Err(error)) => {
                    tracing::debug!("tls handshake failed: {error}");
                    None
                }
                Err(_) => {
                    tracing::debug!("tls handshake timed out after {HANDSHAKE_TIMEOUT:?}");
                    None
                }
            }
        }
    })
    .await
}

async fn run_listener<IO, Upgrade, Fut>(
    listener: TcpListener,
    router: Router,
    limits: ListenLimits,
    shutdown: CancellationToken,
    upgrade: Upgrade,
) -> std::io::Result<()>
where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    Upgrade: Fn(TcpStream) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Option<IO>> + Send + 'static,
{
    let slots = Arc::new(Semaphore::new(limits.max_connections()));
    let mut make_service = router.into_make_service_with_connect_info::<SocketAddr>();
    let tracker = TaskTracker::new();
    let upgrade = Arc::new(upgrade);

    loop {
        let accepted = tokio::select! {
            () = shutdown.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(pair) => pair,
            Err(error) if is_connection_error(&error) => continue,
            Err(error) => {
                tracing::warn!("tcp accept failed, backing off: {error}");
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    () = tokio::time::sleep(ACCEPT_BACKOFF) => {}
                }
                continue;
            }
        };
        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            continue;
        };
        let service = match make_service.call(peer).await {
            Ok(service) => service,
            Err(never) => match never {},
        };
        let header_timeout = limits.header_timeout().get();
        let conn_shutdown = shutdown.clone();
        let upgrade = Arc::clone(&upgrade);
        tracker.spawn(async move {
            let _slot = slot;
            let Some(io) = upgrade(stream).await else {
                return;
            };
            let hyper_service = service_fn(move |request: hyper::Request<Incoming>| {
                service.clone().oneshot(request.map(Body::new))
            });
            let budget = limits.connection_budget();
            let mut builder = Builder::new(TokioExecutor::new());
            builder
                .http1()
                .timer(TokioTimer::new())
                .header_read_timeout(header_timeout);
            builder
                .http2()
                .timer(TokioTimer::new())
                .max_concurrent_streams(budget.max_concurrent_streams().get())
                .initial_stream_window_size(budget.stream_receive_window())
                .initial_connection_window_size(budget.connection_receive_window())
                .keep_alive_interval(Some(header_timeout))
                .keep_alive_timeout(header_timeout);
            let connection =
                builder.serve_connection_with_upgrades(TokioIo::new(io), hyper_service);
            tokio::pin!(connection);

            tokio::select! {
                served = connection.as_mut() => drop(served),
                () = conn_shutdown.cancelled() => {
                    connection.as_mut().graceful_shutdown();
                    let _ = tokio::time::timeout(CONNECTION_DRAIN_GRACE, connection.as_mut()).await;
                }
            }
        });
    }

    tracker.close();
    let _ = tokio::time::timeout(LISTENER_DRAIN_GRACE, tracker.wait()).await;
    Ok(())
}

fn is_connection_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::num::{NonZeroU32, NonZeroU64};

    use axum::routing::get;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    fn limits(header_timeout_ms: u64, max_connections: u32) -> ListenLimits {
        ListenLimits::new(
            crate::limits::HeaderTimeout::from_millis(NonZeroU64::new(header_timeout_ms).unwrap()),
            crate::limits::IdleTimeout::from_millis(NonZeroU64::new(30_000).unwrap()),
            NonZeroU32::new(max_connections).unwrap(),
        )
    }

    async fn bind_and_serve(limits: ListenLimits) -> (SocketAddr, CancellationToken) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route("/", get(|| async { "ok" }));
        let shutdown = CancellationToken::new();
        tokio::spawn(serve_plaintext(listener, router, limits, shutdown.clone()));
        (addr, shutdown)
    }

    async fn read_to_close(stream: &mut TcpStream) -> Vec<u8> {
        let mut collected = Vec::new();
        stream.read_to_end(&mut collected).await.unwrap();
        collected
    }

    #[tokio::test]
    async fn a_full_request_is_answered() {
        let (addr, _shutdown) = bind_and_serve(limits(5_000, 4)).await;
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nhost: oyster.cafe\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
        let answer = read_to_close(&mut stream).await;
        let head = String::from_utf8_lossy(&answer);
        assert!(head.starts_with("HTTP/1.1 200"), "got: {head}");
    }

    #[tokio::test]
    async fn a_slowloris_connection_is_cut_at_the_header_timeout() {
        let (addr, _shutdown) = bind_and_serve(limits(200, 4)).await;
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(b"GET / HTT").await.unwrap();
        let closed = tokio::time::timeout(Duration::from_secs(5), read_to_close(&mut stream))
            .await
            .expect("server cuts connection instead of waiting forever");
        let head = String::from_utf8_lossy(&closed);
        assert!(
            !head.contains("200"),
            "half-sent request must never be answered, got: {head}"
        );
    }

    #[tokio::test]
    async fn an_idle_keep_alive_connection_is_cut_at_the_header_timeout() {
        let (addr, _shutdown) = bind_and_serve(limits(200, 4)).await;
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nhost: oyster.cafe\r\n\r\n")
            .await
            .unwrap();
        let answer = tokio::time::timeout(Duration::from_secs(5), read_to_close(&mut stream))
            .await
            .expect("idle keep-alive connection is cut after answered request");
        let head = String::from_utf8_lossy(&answer);
        assert!(head.starts_with("HTTP/1.1 200"), "got: {head}");
    }

    #[tokio::test]
    async fn a_connection_beyond_the_limit_is_refused() {
        let (addr, _shutdown) = bind_and_serve(limits(5_000, 1)).await;
        let mut held = TcpStream::connect(addr).await.unwrap();
        held.write_all(b"GET / HTT").await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut refused = TcpStream::connect(addr).await.unwrap();
        refused.write_all(b"GET / HTT").await.unwrap();
        let mut answer = Vec::new();
        let outcome =
            tokio::time::timeout(Duration::from_secs(1), refused.read_to_end(&mut answer))
                .await
                .expect("over-limit connection is dropped at accept instead of held to timeout");
        match outcome {
            Ok(_) => assert!(
                answer.is_empty(),
                "over-limit connection gets no bytes, got: {}",
                String::from_utf8_lossy(&answer)
            ),
            Err(reset) => assert_eq!(reset.kind(), std::io::ErrorKind::ConnectionReset),
        }
    }

    #[tokio::test]
    async fn an_in_flight_request_finishes_during_drain() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                "drained-clean"
            }),
        );
        let shutdown = CancellationToken::new();
        tokio::spawn(serve_plaintext(
            listener,
            router,
            limits(5_000, 4),
            shutdown.clone(),
        ));

        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /slow HTTP/1.1\r\nhost: oyster.cafe\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;
        shutdown.cancel();

        let answer = tokio::time::timeout(Duration::from_secs(5), read_to_close(&mut stream))
            .await
            .expect("an in-flight request is answered through the graceful drain");
        let text = String::from_utf8_lossy(&answer);
        assert!(text.starts_with("HTTP/1.1 200"), "got: {text}");
        assert!(
            text.trim_end().ends_with("drained-clean"),
            "the in-flight response must complete during drain, got: {text}"
        );
    }

    #[tokio::test]
    async fn cancelling_the_token_stops_the_listener() {
        let (addr, shutdown) = bind_and_serve(limits(5_000, 4)).await;
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nhost: oyster.cafe\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
        let _ = read_to_close(&mut stream).await;

        shutdown.cancel();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let refused = TcpStream::connect(addr).await;
        if let Ok(mut late) = refused {
            late.write_all(b"GET / HTTP/1.1\r\nhost: oyster.cafe\r\nconnection: close\r\n\r\n")
                .await
                .ok();
            let mut answer = Vec::new();
            let _ =
                tokio::time::timeout(Duration::from_secs(1), late.read_to_end(&mut answer)).await;
            assert!(
                !String::from_utf8_lossy(&answer).contains("200"),
                "a drained listener mustn't answer new requests"
            );
        }
    }
}

#[cfg(test)]
mod tls_tests {
    use super::*;

    use std::num::{NonZeroU32, NonZeroU64};

    use axum::routing::get;
    use futures::StreamExt;
    use rustls::NamedGroup;
    use rustls::crypto::aws_lc_rs;
    use rustls::pki_types::ServerName;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio_rustls::TlsConnector;

    use crate::tls;

    fn client(alpn: &[&[u8]]) -> TlsConnector {
        client_with_provider(aws_lc_rs::default_provider(), alpn)
    }

    fn client_with_provider(
        provider: rustls::crypto::CryptoProvider,
        alpn: &[&[u8]],
    ) -> TlsConnector {
        let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(tls::test_support::AcceptAnyServerCert))
            .with_no_client_auth();
        config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        TlsConnector::from(Arc::new(config))
    }

    async fn spawn_tls_server(
        resolver: Arc<tls::ReloadableCertResolver>,
        limits: ListenLimits,
    ) -> (SocketAddr, CancellationToken) {
        let server_config = Arc::new(tls::build_tls_server_config(resolver, &[]).unwrap());
        let listener = TcpListener::bind("[::1]:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route("/", get(|| async { "ok" }));
        let shutdown = CancellationToken::new();
        tokio::spawn(serve_tls(
            listener,
            router,
            server_config,
            limits,
            shutdown.clone(),
        ));
        (addr, shutdown)
    }

    async fn serve(
        alpn_offer: &[&[u8]],
    ) -> (
        SocketAddr,
        CancellationToken,
        tokio_rustls::client::TlsStream<TcpStream>,
    ) {
        let (addr, shutdown) =
            spawn_tls_server(tls::test_support::resolver(), tls::test_support::limits()).await;
        let tcp = TcpStream::connect(addr).await.unwrap();
        let name = ServerName::try_from("localhost").unwrap();
        let tls = client(alpn_offer).connect(name, tcp).await.unwrap();
        (addr, shutdown, tls)
    }

    #[tokio::test]
    async fn it_terminates_tls_over_http1_and_negotiates_post_quantum() {
        let (_addr, shutdown, mut tls) = serve(&[b"http/1.1"]).await;

        let group = tls.get_ref().1.negotiated_key_exchange_group().unwrap();
        assert_eq!(
            group.name(),
            NamedGroup::X25519MLKEM768,
            "prefer-post-quantum must select X25519MLKEM768 with a capable client"
        );
        assert_eq!(
            tls.get_ref().1.alpn_protocol(),
            Some(b"http/1.1".as_slice())
        );

        tls.write_all(b"GET / HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        tls.read_to_end(&mut response).await.unwrap();
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 200"), "got: {text}");
        assert!(text.trim_end().ends_with("ok"), "got: {text}");

        shutdown.cancel();
    }

    #[tokio::test]
    async fn it_negotiates_h2_when_the_client_offers_only_h2() {
        let (_addr, shutdown, tls) = serve(&[b"h2"]).await;
        assert_eq!(tls.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
        shutdown.cancel();
    }

    #[tokio::test]
    async fn a_classical_only_client_completes_over_x25519() {
        let (addr, shutdown) =
            spawn_tls_server(tls::test_support::resolver(), tls::test_support::limits()).await;

        let tcp = TcpStream::connect(addr).await.unwrap();
        let name = ServerName::try_from("localhost").unwrap();
        let tls =
            client_with_provider(tls::test_support::classical_only_provider(), &[b"http/1.1"])
                .connect(name, tcp)
                .await
                .unwrap();

        let group = tls.get_ref().1.negotiated_key_exchange_group().unwrap();
        assert_eq!(
            group.name(),
            NamedGroup::X25519,
            "a client without ML-KEM must still complete the handshake over classical X25519"
        );

        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_handshakes_survive_a_cert_reload_under_load() {
        let high_limit = ListenLimits::new(
            crate::limits::HeaderTimeout::from_millis(NonZeroU64::new(5_000).unwrap()),
            crate::limits::IdleTimeout::from_millis(NonZeroU64::new(30_000).unwrap()),
            NonZeroU32::new(512).unwrap(),
        );
        let resolver = tls::test_support::resolver();
        let (addr, shutdown) = spawn_tls_server(resolver.clone(), high_limit).await;

        let churn = {
            let resolver = resolver.clone();
            let stop = shutdown.clone();
            tokio::spawn(async move {
                futures::stream::unfold(0u32, move |swaps| {
                    let resolver = resolver.clone();
                    let stop = stop.clone();
                    async move {
                        match stop.is_cancelled() {
                            true => None,
                            false => {
                                resolver.store(tls::test_support::self_signed());
                                tokio::time::sleep(Duration::from_millis(1)).await;
                                Some((swaps + 1, swaps + 1))
                            }
                        }
                    }
                })
                .fold(0u32, |_, swaps| async move { swaps })
                .await
            })
        };

        let clients: Vec<_> = (0..48)
            .map(|_| {
                tokio::spawn(async move {
                    let tcp = TcpStream::connect(addr).await.unwrap();
                    let name = ServerName::try_from("localhost").unwrap();
                    let mut tls = client(&[b"http/1.1"]).connect(name, tcp).await.unwrap();
                    tls.write_all(
                        b"GET / HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
                    let mut response = Vec::new();
                    tls.read_to_end(&mut response).await.unwrap();
                    let text = String::from_utf8_lossy(&response).into_owned();
                    text.starts_with("HTTP/1.1 200") && text.trim_end().ends_with("ok")
                })
            })
            .collect();

        let outcomes: Vec<bool> = futures::future::join_all(clients)
            .await
            .into_iter()
            .map(|joined| joined.unwrap())
            .collect();
        assert!(
            outcomes.iter().all(|served| *served),
            "every handshake racing a cert reload must complete and serve the router uncorrupted"
        );

        shutdown.cancel();
        assert!(
            churn.await.unwrap() > 1,
            "the resolver must have reloaded the certificate during the load"
        );
    }
}
