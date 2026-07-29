#![allow(dead_code)]

use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, Bytes};
use http::{Method, StatusCode};
use knot_edge::{
    BodyInactivityTimeout, BurstSize, CertSource, EdgeConfig, EdgeGuards, HeaderTimeout,
    IdleTimeout, ListenLimits, MaxInflightRequests, RequestTimeout, RequestsPerSecond,
    RequiresFullHandshake, StaticCertPaths, TlsSetup, WriteRequestTimeout, ZeroRttRoutes,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::aws_lc_rs;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub fn nz32(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).unwrap()
}

pub fn nz64(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
}

pub fn pkt(payload: &[u8]) -> Vec<u8> {
    assert!(
        payload.len() + 4 <= 0xFFF0,
        "pkt-line payload exceeds the 65516-byte maximum"
    );
    let mut out = format!("{:04x}", payload.len() + 4).into_bytes();
    out.extend_from_slice(payload);
    out
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub fn write_self_signed(dir: &Path) -> (PathBuf, PathBuf, Vec<u8>) {
    let generated =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .unwrap();
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    std::fs::write(&cert_path, generated.cert.pem()).unwrap();
    std::fs::write(&key_path, generated.signing_key.serialize_pem()).unwrap();
    let der = generated.cert.der().as_ref().to_vec();
    (cert_path, key_path, der)
}

pub fn edge_config(addr: SocketAddr, cert: PathBuf, key: PathBuf) -> EdgeConfig {
    EdgeConfig {
        http_addr: knot_edge::PublicBind::new(addr),
        limits: ListenLimits::new(
            HeaderTimeout::from_millis(nz64(30_000)),
            IdleTimeout::from_millis(nz64(120_000)),
            nz32(1024),
        ),
        guards: EdgeGuards::new(
            RequestsPerSecond::new(nz32(1_000_000)),
            BurstSize::new(nz32(1_000_000)),
            MaxInflightRequests::new(nz32(10_000)),
            RequestTimeout::from_millis(nz64(120_000)),
            BodyInactivityTimeout::from_millis(nz64(120_000)),
            WriteRequestTimeout::from_millis(nz64(1_800_000)),
            knot_types::ProxyTrust::default(),
        ),
        tls: Some(TlsSetup {
            source: CertSource::Static(StaticCertPaths {
                cert_path: knot_edge::CertChainPath::new(cert),
                key_path: knot_edge::PrivateKeyPath::new(key),
            }),
            http3: true,
            internal: None,
        }),
    }
}

pub struct Edge {
    pub addr: SocketAddr,
    pub shutdown: CancellationToken,
    pub task: JoinHandle<Result<(), knot_edge::EdgeError>>,
    pub client: quinn::Endpoint,
}

async fn probe_identity(
    endpoint: &quinn::Endpoint,
    addr: SocketAddr,
    expected_cert: &[u8],
) -> Option<bool> {
    let connecting = endpoint.connect(addr, "localhost").ok()?;
    let connection = tokio::time::timeout(Duration::from_millis(250), connecting)
        .await
        .ok()?
        .ok()?;
    let ours = connection
        .peer_identity()
        .and_then(|identity| identity.downcast::<Vec<CertificateDer<'static>>>().ok())
        .map(|certs| {
            certs
                .first()
                .is_some_and(|cert| cert.as_ref() == expected_cert)
        })
        .unwrap_or(false);
    connection.close(0u32.into(), b"probe done");
    Some(ours)
}

async fn await_ready(
    endpoint: &quinn::Endpoint,
    addr: SocketAddr,
    task: &mut JoinHandle<Result<(), knot_edge::EdgeError>>,
    expected_cert: &[u8],
) -> bool {
    for _ in 0..200 {
        if task.is_finished() {
            return false;
        }
        if let Some(ours) = probe_identity(endpoint, addr, expected_cert).await {
            return ours;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

pub async fn serve_edge(
    certdir: &Path,
    build: impl Fn() -> (RequiresFullHandshake, ZeroRttRoutes),
) -> Edge {
    for _ in 0..8 {
        let addr: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
        let (cert, key, cert_der) = write_self_signed(certdir);
        let (app, advertisement) = build();
        let shutdown = CancellationToken::new();
        let mut task = tokio::spawn(knot_edge::serve(
            edge_config(addr, cert, key),
            app,
            advertisement,
            shutdown.clone(),
        ));
        let client = h3_client();
        if await_ready(&client, addr, &mut task, &cert_der).await {
            return Edge {
                addr,
                shutdown,
                task,
                client,
            };
        }
        client.close(0u32.into(), b"stand up retry");
        shutdown.cancel();
        let _ = task.await;
    }
    panic!("couldn't bind a free TCP+UDP port for the edge after several attempts");
}

#[derive(Debug)]
struct AcceptAnyServerCert;

impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &aws_lc_rs::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &aws_lc_rs::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

pub fn h3_client() -> quinn::Endpoint {
    let mut crypto =
        rustls::ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert))
            .with_no_client_auth();
    crypto.alpn_protocols = vec![b"h3".to_vec()];
    crypto.resumption = rustls::client::Resumption::disabled();
    let quic = quinn::crypto::rustls::QuicClientConfig::try_from(crypto).unwrap();
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic)));
    endpoint
}

pub async fn drain(
    stream: &mut h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(mut chunk) = stream.recv_data().await.unwrap() {
        out.extend_from_slice(&chunk.copy_to_bytes(chunk.remaining()));
    }
    out
}

pub async fn finish_request(
    stream: &mut h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
) {
    match stream.finish().await {
        Ok(()) => (),
        Err(h3::error::StreamError::RemoteTerminate { code, .. })
            if code == h3::error::Code::H3_NO_ERROR => {}
        Err(error) => panic!("finishing the request stream failed: {error}"),
    }
}

pub async fn h3_request(
    edge: &Edge,
    method: Method,
    uri: String,
    headers: &[(&str, &str)],
    body: Option<Bytes>,
    warmup: Option<&str>,
) -> (StatusCode, Vec<u8>) {
    let connection = edge
        .client
        .connect(edge.addr, "localhost")
        .unwrap()
        .await
        .unwrap();
    let quic = connection.clone();
    let (mut driver, mut sender) = h3::client::new(h3_quinn::Connection::new(connection))
        .await
        .unwrap();
    let drive = tokio::spawn(async move {
        let _ = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
    });

    if let Some(warmup) = warmup {
        let request = http::Request::get(warmup).body(()).unwrap();
        let mut stream = sender.send_request(request).await.unwrap();
        finish_request(&mut stream).await;
        let _ = stream.recv_response().await.unwrap();
        drain(&mut stream).await;
    }

    let request = headers
        .iter()
        .fold(
            http::Request::builder().method(method).uri(uri),
            |builder, (name, value)| builder.header(*name, *value),
        )
        .body(())
        .unwrap();
    let mut stream = sender.send_request(request).await.unwrap();
    if let Some(body) = body {
        stream.send_data(body).await.unwrap();
    }
    finish_request(&mut stream).await;
    let status = stream.recv_response().await.unwrap().status();
    let out = drain(&mut stream).await;
    quic.close(0u32.into(), b"done");
    drive.abort();
    (status, out)
}
