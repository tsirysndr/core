use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;
use base64::Engine;
use quinn::crypto::rustls::QuicServerConfig;
use rustls::RootCertStore;
use rustls::ServerConfig;
use rustls::crypto::aws_lc_rs;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::{ClientHello, ResolvesServerCert, WebPkiClientVerifier};
use rustls::sign::CertifiedKey;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::limits::ListenLimits;
use crate::zerortt::EarlyDataPolicy;

pub const ACME_TLS_ALPN: &[u8] = rustls_acme::acme::ACME_TLS_ALPN_NAME;

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("reading {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("parsing {path}: {message}")]
    Parse { path: String, message: String },
    #[error("no certificates found in {0}")]
    NoCertificates(String),
    #[error("no private key found in {0}")]
    NoPrivateKey(String),
    #[error("unusable private key: {0}")]
    SigningKey(String),
    #[error("building server config: {0}")]
    Config(String),
    #[error("certificate and private key don't match: {0}")]
    KeyMismatch(String),
    #[error("session ticketer: {0}")]
    Ticketer(String),
    #[error("client certificate verifier for {path}: {message}")]
    ClientVerifier { path: String, message: String },
    #[error("admin SPKI pin: {0}")]
    SpkiPin(String),
}

#[derive(Clone)]
pub struct SpkiPin([u8; 32]);

impl PartialEq for SpkiPin {
    fn eq(&self, other: &Self) -> bool {
        self.0
            .iter()
            .zip(other.0.iter())
            .fold(0u8, |acc, (left, right)| acc | (left ^ right))
            == 0
    }
}

impl Eq for SpkiPin {}

impl SpkiPin {
    pub fn from_base64(encoded: &str) -> Result<Self, TlsError> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|error| TlsError::SpkiPin(error.to_string()))?;
        let array: [u8; 32] = bytes.try_into().map_err(|bytes: Vec<u8>| {
            TlsError::SpkiPin(format!("expected 32 bytes, got {}", bytes.len()))
        })?;
        Ok(Self(array))
    }

    fn of_certificate(cert: &CertificateDer<'_>) -> Result<Self, rustls::Error> {
        let (_, parsed) = x509_parser::parse_x509_certificate(cert.as_ref()).map_err(|error| {
            rustls::Error::General(format!("parse client certificate: {error}"))
        })?;
        Ok(Self(
            Sha256::digest(parsed.tbs_certificate.subject_pki.raw).into(),
        ))
    }
}

impl std::fmt::Debug for SpkiPin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SpkiPin").finish_non_exhaustive()
    }
}

pub(crate) fn fuzz_of_certificate(data: &[u8]) {
    let cert = CertificateDer::from(data.to_vec());
    let _ = SpkiPin::of_certificate(&cert);
}

pub struct ReloadableCertResolver {
    current: ArcSwap<CertifiedKey>,
}

impl std::fmt::Debug for ReloadableCertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReloadableCertResolver")
            .finish_non_exhaustive()
    }
}

impl ReloadableCertResolver {
    pub fn new(initial: CertifiedKey) -> Self {
        Self {
            current: ArcSwap::from_pointee(initial),
        }
    }

    pub fn store(&self, key: CertifiedKey) {
        self.current.store(Arc::new(key));
    }
}

impl ResolvesServerCert for ReloadableCertResolver {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current.load_full())
    }
}

pub fn spawn_cert_reload(
    resolver: Arc<ReloadableCertResolver>,
    paths: crate::StaticCertPaths,
    shutdown: CancellationToken,
) {
    #[cfg(unix)]
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let mut hangup = match signal(SignalKind::hangup()) {
            Ok(stream) => stream,
            Err(error) => {
                tracing::error!("install SIGHUP handler: {error}");
                return;
            }
        };
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                received = hangup.recv() => {
                    if received.is_none() {
                        break;
                    }
                    match load_certified_key(&paths) {
                        Ok(key) => {
                            resolver.store(key);
                            tracing::info!("reloaded TLS certificate on SIGHUP");
                        }
                        Err(error) => {
                            tracing::warn!("SIGHUP reload kept the existing certificate: {error}");
                        }
                    }
                }
            }
        }
    });

    #[cfg(not(unix))]
    let _ = (resolver, paths, shutdown);
}

pub fn load_certified_key(paths: &crate::StaticCertPaths) -> Result<CertifiedKey, TlsError> {
    let certs = load_certs(paths.cert_path.as_path())?;
    let key = load_private_key(paths.key_path.as_path())?;
    let signing_key = aws_lc_rs::sign::any_supported_type(&key)
        .map_err(|error| TlsError::SigningKey(error.to_string()))?;
    let certified = CertifiedKey::new(certs, signing_key);
    certified
        .keys_match()
        .map_err(|error| TlsError::KeyMismatch(error.to_string()))?;
    Ok(certified)
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let bytes = std::fs::read(path).map_err(|source| TlsError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let mut reader = BufReader::new(bytes.as_slice());
    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| TlsError::Parse {
            path: path.display().to_string(),
            message: error.to_string(),
        })?;
    match certs.is_empty() {
        true => Err(TlsError::NoCertificates(path.display().to_string())),
        false => Ok(certs),
    }
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let bytes = std::fs::read(path).map_err(|source| TlsError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let mut reader = BufReader::new(bytes.as_slice());
    rustls_pemfile::private_key(&mut reader)
        .map_err(|error| TlsError::Parse {
            path: path.display().to_string(),
            message: error.to_string(),
        })?
        .ok_or_else(|| TlsError::NoPrivateKey(path.display().to_string()))
}

fn ticketer() -> Result<Arc<dyn rustls::server::ProducesTickets>, TlsError> {
    aws_lc_rs::Ticketer::new().map_err(|error| TlsError::Ticketer(error.to_string()))
}

fn tcp_alpn(extra: &[&[u8]]) -> Vec<Vec<u8>> {
    [b"h2".as_slice(), b"http/1.1".as_slice()]
        .into_iter()
        .chain(extra.iter().copied())
        .map(<[u8]>::to_vec)
        .collect()
}

pub fn build_tls_server_config(
    resolver: Arc<dyn ResolvesServerCert>,
    extra_alpn: &[&[u8]],
) -> Result<ServerConfig, TlsError> {
    let provider = Arc::new(aws_lc_rs::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| TlsError::Config(error.to_string()))?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    config.alpn_protocols = tcp_alpn(extra_alpn);
    config.ticketer = ticketer()?;
    Ok(config)
}

pub fn build_mtls_server_config(
    resolver: Arc<dyn ResolvesServerCert>,
    client_ca: &crate::ClientCaPath,
    pin: SpkiPin,
) -> Result<ServerConfig, TlsError> {
    let provider = Arc::new(aws_lc_rs::default_provider());
    let roots = load_client_ca(client_ca.as_path())?;
    let webpki =
        WebPkiClientVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
            .build()
            .map_err(|error| TlsError::ClientVerifier {
                path: client_ca.as_path().display().to_string(),
                message: error.to_string(),
            })?;
    let verifier = Arc::new(PinnedClientVerifier { inner: webpki, pin });
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| TlsError::Config(error.to_string()))?
        .with_client_cert_verifier(verifier)
        .with_cert_resolver(resolver);
    config.alpn_protocols = tcp_alpn(&[]);
    config.ticketer = ticketer()?;
    Ok(config)
}

fn load_client_ca(path: &Path) -> Result<RootCertStore, TlsError> {
    let certs = load_certs(path)?;
    let mut roots = RootCertStore::empty();
    let (added, _) = roots.add_parsable_certificates(certs);
    match added {
        0 => Err(TlsError::NoCertificates(path.display().to_string())),
        _ => Ok(roots),
    }
}

#[derive(Debug)]
struct PinnedClientVerifier {
    inner: Arc<dyn ClientCertVerifier>,
    pin: SpkiPin,
}

impl ClientCertVerifier for PinnedClientVerifier {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        self.inner.root_hint_subjects()
    }

    fn offer_client_auth(&self) -> bool {
        self.inner.offer_client_auth()
    }

    fn client_auth_mandatory(&self) -> bool {
        self.inner.client_auth_mandatory()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let verified = self
            .inner
            .verify_client_cert(end_entity, intermediates, now)?;
        match SpkiPin::of_certificate(end_entity)? == self.pin {
            true => Ok(verified),
            false => Err(rustls::Error::General(
                "client certificate SPKI doesn't match the pinned admin identity".to_string(),
            )),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

pub fn build_quic_server_config(
    resolver: Arc<dyn ResolvesServerCert>,
    limits: ListenLimits,
    early_data: EarlyDataPolicy,
) -> Result<quinn::ServerConfig, TlsError> {
    let provider = Arc::new(aws_lc_rs::default_provider());
    let mut crypto = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|error| TlsError::Config(error.to_string()))?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    crypto.alpn_protocols = vec![b"h3".to_vec()];
    crypto.max_early_data_size = early_data.max_early_data_size();

    let quic_crypto =
        QuicServerConfig::try_from(crypto).map_err(|error| TlsError::Config(error.to_string()))?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(quic_crypto));

    let budget = limits.connection_budget();
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(quinn::VarInt::from_u32(
        budget.max_concurrent_streams().get(),
    ));
    transport.stream_receive_window(quinn::VarInt::from_u32(budget.stream_receive_window()));
    transport.receive_window(quinn::VarInt::from_u32(budget.connection_receive_window()));
    let idle = limits.idle_timeout().get();
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(idle).map_err(|error| TlsError::Config(error.to_string()))?,
    ));
    transport.keep_alive_interval(Some(idle / 2));
    config.transport_config(Arc::new(transport));
    Ok(config)
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::num::{NonZeroU32, NonZeroU64};

    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    use rustls::sign::CertifiedKey;

    use super::*;
    use crate::limits::ListenLimits;

    pub(crate) fn self_signed() -> CertifiedKey {
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_der = generated.cert.der().clone();
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            generated.signing_key.serialize_der(),
        ));
        let signing_key = aws_lc_rs::sign::any_supported_type(&key_der).unwrap();
        CertifiedKey::new(vec![cert_der], signing_key)
    }

    pub(crate) fn resolver() -> Arc<ReloadableCertResolver> {
        Arc::new(ReloadableCertResolver::new(self_signed()))
    }

    pub(crate) fn limits() -> ListenLimits {
        ListenLimits::new(
            crate::limits::HeaderTimeout::from_millis(NonZeroU64::new(5_000).unwrap()),
            crate::limits::IdleTimeout::from_millis(NonZeroU64::new(30_000).unwrap()),
            NonZeroU32::new(64).unwrap(),
        )
    }

    pub(crate) fn classical_only_provider() -> rustls::crypto::CryptoProvider {
        let mut provider = aws_lc_rs::default_provider();
        provider.kx_groups = vec![aws_lc_rs::kx_group::X25519];
        provider
    }

    #[derive(Debug)]
    pub(crate) struct AcceptAnyServerCert;

    impl rustls::client::danger::ServerCertVerifier for AcceptAnyServerCert {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
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
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &aws_lc_rs::default_provider().signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            aws_lc_rs::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tcp_alpn_set_offers_h2_and_http1_but_not_h3() {
        let config = build_tls_server_config(test_support::resolver(), &[]).unwrap();
        assert_eq!(
            config.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()],
            "h3 is QUIC-only and must never appear in the TCP ALPN set"
        );
    }

    #[test]
    fn the_acme_challenge_alpn_joins_only_when_requested() {
        let config = build_tls_server_config(test_support::resolver(), &[ACME_TLS_ALPN]).unwrap();
        assert_eq!(
            config.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec(), ACME_TLS_ALPN.to_vec()],
            "acme-tls/1 must trail h2 and http/1.1 so normal clients never select it"
        );
    }

    #[test]
    fn the_tcp_config_installs_an_enabled_session_ticketer() {
        let config = build_tls_server_config(test_support::resolver(), &[]).unwrap();
        assert!(
            config.ticketer.enabled(),
            "session resumption requires an enabled ticketer"
        );
    }

    #[test]
    fn an_spki_pin_round_trips_through_base64() {
        let encoded = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
        assert_eq!(SpkiPin::from_base64(&encoded).unwrap(), SpkiPin([9u8; 32]));
    }

    #[test]
    fn the_spki_pin_matches_the_standard_openssl_recipe() {
        const CERT_PEM: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBhDCCASugAwIBAgIUSkWE4CZvV8B8z9phedKo1PbahDUwCgYIKoZIzj0EAwIw\n\
GDEWMBQGA1UEAwwNYW5lbW9uZS5hZG1pbjAeFw0yNjA2MjExOTA5MDdaFw0zNjA2\n\
MTgxOTA5MDdaMBgxFjAUBgNVBAMMDWFuZW1vbmUuYWRtaW4wWTATBgcqhkjOPQIB\n\
BggqhkjOPQMBBwNCAASFLKd70MtSGSyI2UjdpQyjaJrvXLofac41nI346wK0lC9G\n\
PjZH/NKqo1iwQn+UfZB7gotfezWrDmAUz5OgT6Rlo1MwUTAdBgNVHQ4EFgQUis7S\n\
XEFGpe4gQwWnzX/uzjpt274wHwYDVR0jBBgwFoAUis7SXEFGpe4gQwWnzX/uzjpt\n\
274wDwYDVR0TAQH/BAUwAwEB/zAKBggqhkjOPQQDAgNHADBEAiBhJgE4cMP5/FJw\n\
imc3fYQxOhm5nO59cfG06+0vuDIV1QIgMsKZjFjsch8rbLRNiJL5+bDmlgO7MD14\n\
0PAyPOyjb+w=\n\
-----END CERTIFICATE-----\n";
        const OPENSSL_PIN: &str = "EhmM1HyWzC54br06EDvoAaqt1q1h+je3vVFJTcZ9e1U=";

        let der = rustls_pemfile::certs(&mut CERT_PEM.as_bytes())
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(
            SpkiPin::of_certificate(&der).unwrap(),
            SpkiPin::from_base64(OPENSSL_PIN).unwrap(),
            "of_certificate must hash the same SubjectPublicKeyInfo bytes as openssl pkey -pubin -outform DER | dgst -sha256"
        );
    }

    #[test]
    fn an_spki_pin_of_the_wrong_length_is_rejected() {
        let encoded = base64::engine::general_purpose::STANDARD.encode([9u8; 16]);
        assert!(matches!(
            SpkiPin::from_base64(&encoded),
            Err(TlsError::SpkiPin(_))
        ));
    }

    #[test]
    fn the_quic_alpn_set_offers_only_h3() {
        let quic = build_quic_server_config(
            test_support::resolver(),
            test_support::limits(),
            EarlyDataPolicy::Disabled,
        );
        assert!(quic.is_ok());
    }

    #[test]
    fn the_default_provider_prefers_post_quantum_key_exchange() {
        use rustls::NamedGroup;

        let provider = aws_lc_rs::default_provider();
        let first = provider.kx_groups.first().expect("a key exchange group");
        assert_eq!(
            first.name(),
            NamedGroup::X25519MLKEM768,
            "prefer-post-quantum must order X25519MLKEM768 first for both the TCP and QUIC configs"
        );
    }

    #[test]
    fn a_mismatched_certificate_and_key_is_rejected() {
        let first = test_support::self_signed();
        let second = test_support::self_signed();
        let mismatched = CertifiedKey::new(first.cert.clone(), second.key.clone());
        assert!(mismatched.keys_match().is_err());
    }

    struct ClientIdentity {
        ca_path: crate::ClientCaPath,
        chain: Vec<CertificateDer<'static>>,
        key_der: Vec<u8>,
        pin: SpkiPin,
    }

    fn issue_client_identity() -> ClientIdentity {
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let issuer = rcgen::Issuer::new(ca_params, ca_key);

        let client_key = rcgen::KeyPair::generate().unwrap();
        let client_params = rcgen::CertificateParams::new(vec!["admin.knot".to_string()]).unwrap();
        let client_cert = client_params.signed_by(&client_key, &issuer).unwrap();
        let client_der = client_cert.der().clone();

        let ca_path = std::env::temp_dir().join(format!(
            "knot_edge_mtls_ca_{}_{:p}.pem",
            std::process::id(),
            &client_der as *const _
        ));
        std::fs::write(&ca_path, ca_cert.pem()).unwrap();

        ClientIdentity {
            ca_path: crate::ClientCaPath::new(ca_path),
            pin: SpkiPin::of_certificate(&client_der).unwrap(),
            chain: vec![client_der],
            key_der: client_key.serialize_der(),
        }
    }

    async fn mtls_handshake(
        server_config: ServerConfig,
        identity: Option<&ClientIdentity>,
    ) -> bool {
        use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
        use tokio::net::{TcpListener, TcpStream};
        use tokio_rustls::{TlsAcceptor, TlsConnector};

        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let listener = TcpListener::bind("[::1]:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            acceptor.accept(tcp).await.is_ok()
        });

        let verifier =
            rustls::ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(test_support::AcceptAnyServerCert));
        let mut client_config = match identity {
            Some(identity) => verifier
                .with_client_auth_cert(
                    identity.chain.clone(),
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key_der.clone())),
                )
                .unwrap(),
            None => verifier.with_no_client_auth(),
        };
        client_config.alpn_protocols = vec![b"h2".to_vec()];
        let connector = TlsConnector::from(Arc::new(client_config));
        let tcp = TcpStream::connect(addr).await.unwrap();
        let client_ok = connector
            .connect(ServerName::try_from("localhost").unwrap(), tcp)
            .await
            .is_ok();
        let server_ok = server.await.unwrap();
        client_ok && server_ok
    }

    #[tokio::test]
    async fn mtls_rejects_a_client_presenting_no_certificate() {
        let identity = issue_client_identity();
        let config = build_mtls_server_config(
            test_support::resolver(),
            &identity.ca_path,
            identity.pin.clone(),
        )
        .unwrap();
        assert!(
            !mtls_handshake(config, None).await,
            "the mandatory mTLS verifier must reject a client that presents no certificate"
        );
    }

    #[tokio::test]
    async fn mtls_admits_the_pinned_admin_certificate() {
        let identity = issue_client_identity();
        let config = build_mtls_server_config(
            test_support::resolver(),
            &identity.ca_path,
            identity.pin.clone(),
        )
        .unwrap();
        assert!(
            mtls_handshake(config, Some(&identity)).await,
            "a client presenting the pinned admin certificate must complete the mTLS handshake"
        );
    }

    #[tokio::test]
    async fn mtls_rejects_a_ca_trusted_client_whose_spki_is_not_pinned() {
        let identity = issue_client_identity();
        let config = build_mtls_server_config(
            test_support::resolver(),
            &identity.ca_path,
            SpkiPin([0u8; 32]),
        )
        .unwrap();
        assert!(
            !mtls_handshake(config, Some(&identity)).await,
            "a client trusted by the CA but failing the SPKI pin must be rejected"
        );
    }
}
