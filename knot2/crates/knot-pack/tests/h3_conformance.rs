use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{Buf, Bytes};
use http::{HeaderMap, Method, Uri};
use knot_edge::{
    BodyInactivityTimeout, BurstSize, CertSource, EdgeConfig, EdgeGuards, HeaderTimeout,
    IdleTimeout, ListenLimits, MaxInflightRequests, RequestTimeout, RequestsPerSecond,
    RequiresFullHandshake, StaticCertPaths, TlsSetup, WriteRequestTimeout,
};
use knot_git::Layout;
use knot_pack::{CacheConfig, RepoLookup, RepoResolver, RepoTarget};
use knot_types::{ObjectFormat, RepoDid};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::aws_lc_rs;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

mod common;
use common::{must, pack_objects, receive_request};

type Captured = (Method, Uri, HeaderMap, Bytes);

fn nz32(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).unwrap()
}

fn nz64(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
}

fn object_set(dir: &Path) -> BTreeSet<String> {
    must(
        dir,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    )
    .lines()
    .map(|line| line.trim().to_string())
    .filter(|line| !line.is_empty())
    .collect()
}

fn serve_dids() -> Arc<dyn RepoResolver> {
    Arc::new(|target: &RepoTarget| match target {
        RepoTarget::Did(did) => RepoLookup::Hosted(did.clone()),
        RepoTarget::OwnerRkey(_, _) => RepoLookup::Unhosted,
    })
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn write_self_signed(dir: &Path) -> (PathBuf, PathBuf, Vec<u8>) {
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

fn edge_config(addr: SocketAddr, cert: PathBuf, key: PathBuf) -> EdgeConfig {
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
            None,
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

struct Edge {
    addr: SocketAddr,
    shutdown: CancellationToken,
    log: Arc<Mutex<Vec<Captured>>>,
    task: JoinHandle<Result<(), knot_edge::EdgeError>>,
    client: quinn::Endpoint,
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

async fn stand_up(layout: Layout, certdir: &Path) -> Edge {
    static TRACE: std::sync::Once = std::sync::Once::new();
    TRACE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();
    });
    for _ in 0..8 {
        let addr: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
        let (cert, key, cert_der) = write_self_signed(certdir);
        let (write_routes, advertisement) = knot_pack::edge_routes(
            layout.clone(),
            serve_dids(),
            None,
            None,
            knot_resource::PackSlots::new(4),
            CacheConfig::default(),
            Arc::new(knot_messages::Catalog::defaults()),
            knot_pack::default_hostname().clone(),
            Arc::new(knot_runtime::SystemClock),
        );
        let log: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        let recorded = write_routes.layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let sink = sink.clone();
                async move {
                    let (parts, body) = request.into_parts();
                    let bytes = axum::body::to_bytes(body, usize::MAX)
                        .await
                        .unwrap_or_default();
                    sink.lock().unwrap().push((
                        parts.method.clone(),
                        parts.uri.clone(),
                        parts.headers.clone(),
                        bytes.clone(),
                    ));
                    next.run(axum::extract::Request::from_parts(
                        parts,
                        axum::body::Body::from(bytes),
                    ))
                    .await
                }
            },
        ));
        let app = RequiresFullHandshake::new(recorded);
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
                log,
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

fn h3_client() -> quinn::Endpoint {
    let mut crypto =
        rustls::ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert))
            .with_no_client_auth();
    crypto.alpn_protocols = vec![b"h3".to_vec()];
    let quic = quinn::crypto::rustls::QuicClientConfig::try_from(crypto).unwrap();
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic)));
    endpoint
}

async fn drain(
    stream: &mut h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(mut chunk) = stream.recv_data().await.unwrap() {
        out.extend_from_slice(&chunk.copy_to_bytes(chunk.remaining()));
    }
    out
}

async fn finish_request(
    stream: &mut h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
) {
    match stream.finish().await {
        Ok(()) => (),
        Err(h3::error::StreamError::RemoteTerminate { code, .. })
            if code == h3::error::Code::H3_NO_ERROR => {}
        Err(error) => panic!("finishing the request stream failed: {error}"),
    }
}

async fn replay_over_h3(edge: &Edge, did: &str, request: &Captured) -> Vec<u8> {
    let (_, uri, headers, body) = request;
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

    let warmup = http::Request::get(format!(
        "https://localhost/{did}/info/refs?service=git-upload-pack"
    ))
    .header("git-protocol", "version=2")
    .body(())
    .unwrap();
    let mut warm = sender.send_request(warmup).await.unwrap();
    finish_request(&mut warm).await;
    assert!(
        warm.recv_response().await.unwrap().status().is_success(),
        "h3 info/refs advertisement must serve over QUIC"
    );
    drain(&mut warm).await;

    let mut builder = http::Request::builder()
        .method(Method::POST)
        .uri(format!("https://localhost{}", uri.path()));
    for name in ["content-type", "content-encoding", "git-protocol", "accept"] {
        if let Some(value) = headers.get(name) {
            builder = builder.header(name, value);
        }
    }
    let mut stream = sender
        .send_request(builder.body(()).unwrap())
        .await
        .unwrap();
    stream.send_data(body.clone()).await.unwrap();
    finish_request(&mut stream).await;
    let response = stream.recv_response().await.unwrap();
    assert!(
        response.status().is_success(),
        "h3 upload-pack returned {}",
        response.status()
    );
    let out = drain(&mut stream).await;
    quic.close(0u32.into(), b"done");
    drive.abort();
    out
}

fn fetch_request(log: &Arc<Mutex<Vec<Captured>>>) -> Captured {
    let captured = log.lock().unwrap();
    captured
        .iter()
        .find(|(method, uri, _, body)| {
            method == Method::POST
                && uri.path().ends_with("/git-upload-pack")
                && body
                    .windows(b"command=fetch".len())
                    .any(|window| window == b"command=fetch")
        })
        .cloned()
        .unwrap_or_else(|| {
            let summary: Vec<String> = captured
                .iter()
                .map(|(method, uri, headers, body)| {
                    format!(
                        "{method} {uri} git-protocol={:?} body[..64]={:?}",
                        headers.get("git-protocol"),
                        String::from_utf8_lossy(&body[..body.len().min(64)])
                    )
                })
                .collect();
            panic!("git issued no protocol-v2 fetch over the TLS edge, captured: {summary:#?}")
        })
}

fn extract_pack(response: &[u8]) -> Vec<u8> {
    let mut channel = Vec::new();
    let mut pos = 0usize;
    while pos + 4 <= response.len() {
        let len = std::str::from_utf8(&response[pos..pos + 4])
            .ok()
            .and_then(|hex| usize::from_str_radix(hex, 16).ok())
            .unwrap_or(0);
        pos += 4;
        if len < 4 {
            continue;
        }
        let end = (pos + len - 4).min(response.len());
        let payload = &response[pos..end];
        pos = end;
        if payload.first() == Some(&1) {
            channel.extend_from_slice(&payload[1..]);
        }
    }
    match channel.windows(4).position(|window| window == b"PACK") {
        Some(start) => channel.split_off(start),
        None => channel,
    }
}

fn index_pack(repo: &Path, pack: &[u8]) {
    let (indexed, report) =
        knot_fixtures::feed(repo, &["index-pack", "--stdin", "--fix-thin"], pack);
    assert!(indexed, "index-pack failed: {report}");
}

fn advance_knot(bare: &Path, work: &Path, old: &str, new: &str) {
    let oids: Vec<String> = must(work, &["rev-list", "--objects", new, "--not", old])
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect();
    let request = receive_request("refs/heads/main", old, new, &pack_objects(work, &oids));
    let repo = knot_git::Repo::open(bare).expect("open knot bare");
    let report = knot_pack::receive_pack(&repo, &request).expect("knot receive");
    assert!(
        String::from_utf8_lossy(&report).contains("ok refs/heads/main"),
        "knot must accept a receive that advances main"
    );
}

fn init(dir: &Path, format: ObjectFormat) {
    std::fs::create_dir_all(dir).unwrap();
    let fmt = format!("--object-format={}", format.capability());
    must(dir, &["init", &fmt, "-q", dir.to_str().unwrap()]);
}

fn seed(work: &Path, bares: [&Path; 2], format: ObjectFormat) {
    let fmt = format!("--object-format={}", format.capability());
    std::fs::create_dir_all(work).unwrap();
    must(work, &["init", &fmt, "-q", "-b", "main"]);
    std::fs::write(work.join("README.md"), "h3 conformance\n").unwrap();
    must(work, &["add", "-A"]);
    must(work, &["commit", "-q", "-m", "c1"]);
    let c1 = must(work, &["rev-parse", "HEAD"]);
    std::fs::write(work.join("src.txt"), "more\n").unwrap();
    must(work, &["add", "-A"]);
    must(work, &["commit", "-q", "-m", "c2"]);
    must(work, &["checkout", "-q", "-b", "dev", &c1]);
    std::fs::write(work.join("dev.txt"), "branch\n").unwrap();
    must(work, &["add", "-A"]);
    must(work, &["commit", "-q", "-m", "c3"]);
    must(work, &["checkout", "-q", "main"]);
    must(work, &["tag", "-a", "v1", "-m", "release"]);
    bares.into_iter().for_each(|bare| {
        must(
            work,
            &["push", "-q", bare.to_str().unwrap(), "main", "dev", "v1"],
        );
        must(bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    });
}

async fn h3_serves_the_canonical_object_set(format: ObjectFormat, did_str: &str) {
    let scan = tempfile::tempdir().unwrap();
    let certdir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let canon_root = tempfile::tempdir().unwrap();

    let did = RepoDid::new(did_str).unwrap();
    let layout = Layout::new(scan.path()).with_object_format(format);
    layout.create(&did).unwrap();
    let knot_bare = layout.repo_path(&did).unwrap();
    let canon_bare = canon_root.path().join("canon.git");
    let fmt = format!("--object-format={}", format.capability());
    must(
        canon_root.path(),
        &["init", "--bare", &fmt, "-q", canon_bare.to_str().unwrap()],
    );

    let work = scratch.path().join("work");
    seed(&work, [&knot_bare, &canon_bare], format);

    let canon_url = format!("file://{}", canon_bare.to_str().unwrap());
    let canon_clone = scratch.path().join("canon-clone");
    must(
        scratch.path(),
        &["clone", "-q", &canon_url, canon_clone.to_str().unwrap()],
    );
    let canonical = object_set(&canon_clone);

    let edge = stand_up(layout, certdir.path()).await;
    let url = format!("https://{}/{}", edge.addr, did.as_str());

    let h1_clone = scratch.path().join("h1-clone");
    must(
        scratch.path(),
        &[
            "-c",
            "http.sslVerify=false",
            "clone",
            "-q",
            &url,
            h1_clone.to_str().unwrap(),
        ],
    );
    must(&h1_clone, &["config", "http.sslVerify", "false"]);
    assert_eq!(
        must(&canon_clone, &["rev-parse", "HEAD^{tree}"]),
        must(&h1_clone, &["rev-parse", "HEAD^{tree}"]),
        "{format:?} h1/h2 TLS clone checks out the canonical tree"
    );
    assert_eq!(
        canonical,
        object_set(&h1_clone),
        "{format:?} h1/h2 TLS clone transfers the canonical object set"
    );

    let h3_clone = scratch.path().join("h3-clone");
    init(&h3_clone, format);
    let pack = extract_pack(&replay_over_h3(&edge, did.as_str(), &fetch_request(&edge.log)).await);
    index_pack(&h3_clone, &pack);
    assert_eq!(
        canonical,
        object_set(&h3_clone),
        "{format:?} h3 clone over QUIC transfers the canonical object set"
    );

    let old = must(&work, &["rev-parse", "HEAD"]);
    std::fs::write(work.join("incremental.txt"), "fetch me\n").unwrap();
    must(&work, &["add", "-A"]);
    must(&work, &["commit", "-q", "-m", "c4"]);
    let new = must(&work, &["rev-parse", "HEAD"]);
    advance_knot(&knot_bare, &work, &old, &new);
    must(&work, &["push", "-q", canon_bare.to_str().unwrap(), "main"]);

    let canon_after = scratch.path().join("canon-after");
    must(
        scratch.path(),
        &["clone", "-q", &canon_url, canon_after.to_str().unwrap()],
    );
    let canonical_after = object_set(&canon_after);

    edge.log.lock().unwrap().clear();
    must(&h1_clone, &["fetch", "-q", "origin"]);
    assert_eq!(
        canonical_after,
        object_set(&h1_clone),
        "{format:?} h1/h2 TLS fetch advances to the canonical object set"
    );

    let fetch_pack =
        extract_pack(&replay_over_h3(&edge, did.as_str(), &fetch_request(&edge.log)).await);
    index_pack(&h3_clone, &fetch_pack);
    assert_eq!(
        canonical_after,
        object_set(&h3_clone),
        "{format:?} h3 incremental fetch over QUIC advances to the canonical object set"
    );

    edge.shutdown.cancel();
    edge.task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_git_superset_guarantee_holds_over_h3_in_both_object_formats() {
    h3_serves_the_canonical_object_set(ObjectFormat::SHA1, "did:plc:squid").await;
    h3_serves_the_canonical_object_set(ObjectFormat::SHA256, "did:plc:cuttle").await;
}
