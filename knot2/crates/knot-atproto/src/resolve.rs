use std::borrow::Cow;

use knot_runtime::PublicKeyBytes;
use knot_types::crypto::{KeyCodec, PublicKey as CryptoKey};
use knot_types::did_doc::DidDocument;
use knot_types::{AccountDid, Handle, HttpStatus, RepoDid};
use url::{Host, Url};

const LEGACY_K256_KIND: &str = "EcdsaSecp256k1VerificationKey2019";
const LEGACY_P256_KIND: &str = "EcdsaSecp256r1VerificationKey2019";

#[derive(Debug, Clone, thiserror::Error)]
pub enum ResolveError {
    #[error("unsupported DID method in {value:?}")]
    UnsupportedMethod { value: String },
    #[error("DID {value:?} doesn't form a resolvable document location")]
    Unresolvable { value: String },
    #[error("DID document fetch returned HTTP {status}")]
    Status { status: HttpStatus },
    #[error("network failure resolving DID document: {0}")]
    Network(#[from] knot_runtime::NetworkError),
    #[error("DID document isn't valid JSON: {0}")]
    Malformed(String),
    #[error("DID document for {requested:?} claims to describe {document:?}")]
    IdMismatch { requested: String, document: String },
    #[error("refusing to resolve over non-https endpoint: {url}")]
    InsecureScheme { url: String },
    #[error("refusing to resolve non-public address {host}")]
    BlockedHost { host: String },
    #[error("DID document declares no atproto signing key")]
    MissingSigningKey,
    #[error("DID document signing key is unusable: {0}")]
    BadSigningKey(String),
    #[error("DID document declares no atproto_pds service endpoint")]
    MissingPds,
    #[error("atproto_pds endpoint {value:?} isn't valid URL")]
    BadPds { value: String },
    #[error("PLC directory {value:?} isn't valid http(s) base URL")]
    BadPlcDirectory { value: String },
    #[error("identity {did} recently failed to resolve and is negatively cached")]
    RecentlyFailed { did: AccountDid },
    #[error("did:web document for {did} doesn't publish expected signing key")]
    ExpectedKeyAbsent { did: RepoDid },
    #[error("handle {handle} has no atproto DNS or well-known record")]
    HandleUnresolvable { handle: Handle },
    #[error("handle {handle} resolves to more than one distinct DID")]
    HandleAmbiguous { handle: Handle },
    #[error("handle {handle} points at {value:?}, which isn't a valid DID")]
    HandleForwardMalformed { handle: Handle, value: String },
    #[error("handle {handle} resolved to {resolved} but that document claims {claimed:?}")]
    HandleMismatch {
        handle: Handle,
        resolved: AccountDid,
        claimed: Option<Handle>,
    },
    #[error("handle {handle} recently failed to resolve and is negatively cached")]
    HandleRecentlyFailed { handle: Handle },
}

impl ResolveError {
    pub fn is_transient(&self) -> bool {
        match self {
            ResolveError::Network(_) => true,
            ResolveError::Status { status } => status.is_transient(),
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PdsEndpoint(Url);

impl PdsEndpoint {
    pub fn new(url: Url) -> Result<Self, ResolveError> {
        match http_base(&url) {
            true => Ok(Self(url)),
            false => Err(ResolveError::BadPds {
                value: url.as_str().to_string(),
            }),
        }
    }

    pub fn url(&self) -> &Url {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct PlcDirectory(Url);

impl PlcDirectory {
    pub fn new(url: Url) -> Result<Self, ResolveError> {
        match http_base(&url) {
            true => Ok(Self(url)),
            false => Err(ResolveError::BadPlcDirectory {
                value: url.as_str().to_string(),
            }),
        }
    }
}

fn http_base(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.has_host()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}

#[derive(Debug, Clone)]
pub struct Identity {
    pub did: AccountDid,
    pub handles: Vec<Handle>,
    pub signing_key: CryptoKey<'static>,
    pub pds: PdsEndpoint,
}

impl Identity {
    pub fn primary_handle(&self) -> Option<&Handle> {
        self.handles.first()
    }

    pub fn claims_handle(&self, handle: &Handle) -> bool {
        self.handles.iter().any(|known| known == handle)
    }
}

pub(crate) fn document_url(
    did: &AccountDid,
    plc_directory: &PlcDirectory,
) -> Result<Url, ResolveError> {
    let value = did.as_str();
    if value.strip_prefix("did:plc:").is_some() {
        let base = plc_directory.0.as_str().trim_end_matches('/');
        Url::parse(&format!("{base}/{value}")).map_err(|_| ResolveError::Unresolvable {
            value: value.to_string(),
        })
    } else if let Some(rest) = value.strip_prefix("did:web:") {
        web_document_url(rest).ok_or(ResolveError::Unresolvable {
            value: value.to_string(),
        })
    } else {
        Err(ResolveError::UnsupportedMethod {
            value: value.to_string(),
        })
    }
}

pub(crate) fn guard_fetch_url(url: &Url) -> Result<(), ResolveError> {
    if url.scheme() != "https" {
        return Err(ResolveError::InsecureScheme {
            url: url.as_str().to_string(),
        });
    }
    let blocked = match url.host() {
        Some(Host::Ipv4(ip)) => knot_runtime::is_blocked_ip(ip.into()).then(|| ip.to_string()),
        Some(Host::Ipv6(ip)) => knot_runtime::is_blocked_ip(ip.into()).then(|| ip.to_string()),
        _ => None,
    };
    match blocked {
        Some(host) => Err(ResolveError::BlockedHost { host }),
        None => Ok(()),
    }
}

fn web_document_url(rest: &str) -> Option<Url> {
    let mut segments = rest.split(':');
    let authority = segments.next().filter(|head| !head.is_empty())?;
    let host = authority.replace("%3A", ":").replace("%3a", ":");
    let path: Vec<&str> = segments.collect();
    let tail = if path.is_empty() {
        ".well-known/did.json".to_string()
    } else if path.iter().any(|segment| segment.is_empty()) {
        return None;
    } else {
        format!("{}/did.json", path.join("/"))
    };
    Url::parse(&format!("https://{host}/{tail}")).ok()
}

pub(crate) fn identity_from_document(
    did: &AccountDid,
    body: &[u8],
) -> Result<Identity, ResolveError> {
    let document: DidDocument =
        serde_json::from_slice(body).map_err(|error| ResolveError::Malformed(error.to_string()))?;
    if AccountDid::new(document.id.as_str()).ok().as_ref() != Some(did) {
        return Err(ResolveError::IdMismatch {
            requested: did.as_str().to_string(),
            document: document.id.as_str().to_string(),
        });
    }
    let signing_key = atproto_signing_key(&document)?;
    let pds_endpoint = document.pds_endpoint().ok_or(ResolveError::MissingPds)?;
    let pds = Url::parse(pds_endpoint.as_str())
        .map_err(|_| ResolveError::BadPds {
            value: pds_endpoint.as_str().to_string(),
        })
        .and_then(PdsEndpoint::new)?;
    let handles = document
        .handles()
        .iter()
        .filter_map(|found| Handle::new_owned(found.as_str()).ok())
        .collect();
    Ok(Identity {
        did: did.clone(),
        handles,
        signing_key,
        pds,
    })
}

pub(crate) fn web_document_url_for(did: &RepoDid) -> Result<Url, ResolveError> {
    let rest =
        did.as_str()
            .strip_prefix("did:web:")
            .ok_or_else(|| ResolveError::UnsupportedMethod {
                value: did.as_str().to_string(),
            })?;
    web_document_url(rest).ok_or_else(|| ResolveError::Unresolvable {
        value: did.as_str().to_string(),
    })
}

pub(crate) fn document_publishes_key(
    did: &RepoDid,
    body: &[u8],
    expected: &PublicKeyBytes,
) -> Result<(), ResolveError> {
    let document: DidDocument =
        serde_json::from_slice(body).map_err(|error| ResolveError::Malformed(error.to_string()))?;
    if document.id.as_str() != did.as_str() {
        return Err(ResolveError::IdMismatch {
            requested: did.as_str().to_string(),
            document: document.id.as_str().to_string(),
        });
    }
    let methods = document
        .verification_method
        .as_ref()
        .ok_or(ResolveError::MissingSigningKey)?;
    let published = methods
        .iter()
        .filter_map(|method| method_key(method).ok())
        .any(|key| {
            matches!(key.codec, KeyCodec::Secp256k1) && key.bytes.as_ref() == expected.as_bytes()
        });
    if published {
        Ok(())
    } else {
        Err(ResolveError::ExpectedKeyAbsent { did: did.clone() })
    }
}

fn supported_kind(kind: &str) -> bool {
    matches!(kind, "Multikey" | LEGACY_K256_KIND | LEGACY_P256_KIND)
}

fn method_key(
    method: &knot_types::did_doc::VerificationMethod<knot_types::DefaultStr>,
) -> Result<CryptoKey<'static>, String> {
    let multibase = method
        .public_key_multibase
        .as_ref()
        .ok_or_else(|| "verification method lacks publicKeyMultibase".to_string())?
        .as_ref();
    match method.r#type.as_ref() {
        "Multikey" => CryptoKey::decode_owned(multibase).map_err(|error| error.to_string()),
        LEGACY_K256_KIND => legacy_key(KeyCodec::Secp256k1, multibase),
        LEGACY_P256_KIND => legacy_key(KeyCodec::P256, multibase),
        other => Err(format!("unsupported verification method type {other:?}")),
    }
}

fn legacy_key(codec: KeyCodec, multibase: &str) -> Result<CryptoKey<'static>, String> {
    let encoded = multibase
        .strip_prefix('z')
        .ok_or_else(|| format!("legacy key {multibase:?} isn't base58btc multibase"))?;
    let bytes = bs58::decode(encoded)
        .into_vec()
        .map_err(|error| error.to_string())?;
    Ok(CryptoKey {
        codec,
        bytes: Cow::Owned(bytes),
    })
}

fn atproto_signing_key(document: &DidDocument) -> Result<CryptoKey<'static>, ResolveError> {
    let method = document
        .verification_method
        .as_ref()
        .and_then(|methods| {
            methods.iter().find(|method| {
                let id: &str = method.id.as_ref();
                id.ends_with("#atproto")
                    && supported_kind(method.r#type.as_ref())
                    && method.public_key_multibase.is_some()
            })
        })
        .ok_or(ResolveError::MissingSigningKey)?;
    method_key(method).map_err(ResolveError::BadSigningKey)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use bytes::Bytes;

    struct UrlCase {
        did: &'static str,
        expect: fn(&Result<Url, ResolveError>) -> bool,
    }

    const URL_CASES: &[UrlCase] = &[
        UrlCase {
            did: "did:plc:squid",
            expect: |r| matches!(r, Ok(u) if u.as_str() == "https://plc.directory/did:plc:squid"),
        },
        UrlCase {
            did: "did:web:nel.pet",
            expect: |r| matches!(r, Ok(u) if u.as_str() == "https://nel.pet/.well-known/did.json"),
        },
        UrlCase {
            did: "did:web:nel.pet:repos:squid",
            expect: |r| matches!(r, Ok(u) if u.as_str() == "https://nel.pet/repos/squid/did.json"),
        },
        UrlCase {
            did: "did:web:nel.pet%3A8443",
            expect: |r| matches!(r, Ok(u) if u.as_str() == "https://nel.pet:8443/.well-known/did.json"),
        },
        UrlCase {
            did: "did:key:zabc",
            expect: |r| matches!(r, Err(ResolveError::UnsupportedMethod { .. })),
        },
    ];

    #[test]
    fn document_url_maps_each_did_method_to_its_document_location() {
        URL_CASES.iter().for_each(|case| {
            let result = document_url(&did(case.did), &plc());
            assert!((case.expect)(&result), "case {:?} got {result:?}", case.did);
        });
    }

    fn body(value: serde_json::Value) -> Bytes {
        Bytes::from(serde_json::to_vec(&value).unwrap())
    }

    fn sample_key() -> String {
        knot_types::crypto::multikey(0xe7, &sec1(&signer(5)))
    }

    fn legacy_document(kind: &str, multibase: &str) -> Bytes {
        body(serde_json::json!({
            "id": SQUID,
            "alsoKnownAs": ["at://nel.pet"],
            "verificationMethod": [{
                "id": format!("{SQUID}#atproto"),
                "type": kind,
                "controller": SQUID,
                "publicKeyMultibase": multibase
            }],
            "service": [{
                "id": "#atproto_pds",
                "type": "AtprotoPersonalDataServer",
                "serviceEndpoint": "https://pds.oyster.cafe"
            }]
        }))
    }

    #[test]
    fn a_complete_document_yields_a_full_identity() {
        let body = did_doc(DocSpec {
            id: SQUID,
            signing: &signer(5),
            handle: "nel.pet",
            pds: "https://pds.oyster.cafe",
            method: MethodKind::Multikey,
        });
        let identity = identity_from_document(&did(SQUID), &body).unwrap();
        assert_eq!(identity.pds.url().as_str(), "https://pds.oyster.cafe/");
        assert_eq!(identity.primary_handle().unwrap().as_str(), "nel.pet");
    }

    #[test]
    fn the_atproto_verification_method_wins_over_a_decoy_first_key() {
        let decoy = sec1(&signer(3));
        let real = sec1(&signer(5));
        let document = body(serde_json::json!({
            "id": SQUID,
            "alsoKnownAs": ["at://nel.pet"],
            "verificationMethod": [
                {
                    "id": format!("{SQUID}#extra"),
                    "type": "Multikey",
                    "controller": SQUID,
                    "publicKeyMultibase": knot_types::crypto::multikey(0xe7, &decoy)
                },
                {
                    "id": format!("{SQUID}#atproto"),
                    "type": "Multikey",
                    "controller": SQUID,
                    "publicKeyMultibase": knot_types::crypto::multikey(0xe7, &real)
                }
            ],
            "service": [{
                "id": "#atproto_pds",
                "type": "AtprotoPersonalDataServer",
                "serviceEndpoint": "https://pds.oyster.cafe"
            }]
        }));
        let identity = identity_from_document(&did(SQUID), &document).unwrap();
        assert_eq!(identity.signing_key.bytes.as_ref(), real.as_slice());
        assert_ne!(identity.signing_key.bytes.as_ref(), decoy.as_slice());
    }

    #[test]
    fn a_legacy_secp256k1_verification_method_resolves() {
        let body = did_doc(DocSpec {
            id: SQUID,
            signing: &signer(5),
            handle: "nel.pet",
            pds: "https://pds.oyster.cafe",
            method: MethodKind::LegacyK256,
        });
        let identity = identity_from_document(&did(SQUID), &body).unwrap();
        assert_eq!(
            identity.signing_key.bytes.as_ref(),
            sec1(&signer(5)).as_slice()
        );
        assert!(matches!(identity.signing_key.codec, KeyCodec::Secp256k1));
    }

    #[test]
    fn document_publishes_key_matches_only_on_codec_and_bytes() {
        let published = did_doc(DocSpec {
            id: SQUID,
            signing: &signer(5),
            handle: "nel.pet",
            pds: "https://pds.oyster.cafe",
            method: MethodKind::LegacyK256,
        });
        document_publishes_key(
            &RepoDid::new(SQUID).unwrap(),
            &published,
            &PublicKeyBytes::from_bytes(sec1(&signer(5))),
        )
        .unwrap();

        let foreign = did_doc(DocSpec {
            id: SQUID,
            signing: &signer(5),
            handle: "nel.pet",
            pds: "https://pds.oyster.cafe",
            method: MethodKind::LegacyP256,
        });
        let error = document_publishes_key(
            &RepoDid::new(SQUID).unwrap(),
            &foreign,
            &PublicKeyBytes::from_bytes(sec1(&signer(5))),
        )
        .unwrap_err();
        assert!(
            matches!(error, ResolveError::ExpectedKeyAbsent { .. }),
            "the same bytes under a foreign codec mustn't satisfy publishes_key, got {error:?}"
        );
    }

    struct DocCase {
        name: &'static str,
        requested: &'static str,
        body: fn() -> Bytes,
        expect: fn(&Result<Identity, ResolveError>) -> bool,
    }

    const DOC_CASES: &[DocCase] = &[
        DocCase {
            name: "document declares no atproto_pds service",
            requested: SQUID,
            body: || {
                body(serde_json::json!({
                    "id": SQUID,
                    "verificationMethod": [{
                        "id": format!("{SQUID}#atproto"),
                        "type": "Multikey",
                        "controller": SQUID,
                        "publicKeyMultibase": sample_key()
                    }]
                }))
            },
            expect: |r| matches!(r, Err(ResolveError::MissingPds)),
        },
        DocCase {
            name: "document declares no signing key",
            requested: SQUID,
            body: || {
                body(serde_json::json!({
                    "id": SQUID,
                    "service": [{
                        "id": "#atproto_pds",
                        "type": "AtprotoPersonalDataServer",
                        "serviceEndpoint": "https://pds.oyster.cafe"
                    }]
                }))
            },
            expect: |r| matches!(r, Err(ResolveError::MissingSigningKey)),
        },
        DocCase {
            name: "body isn't valid json",
            requested: SQUID,
            body: || Bytes::from_static(b"not json"),
            expect: |r| matches!(r, Err(ResolveError::Malformed(_))),
        },
        DocCase {
            name: "legacy key without a multibase prefix",
            requested: SQUID,
            body: || {
                legacy_document(
                    "EcdsaSecp256k1VerificationKey2019",
                    &bs58::encode(sec1(&signer(5))).into_string(),
                )
            },
            expect: |r| matches!(r, Err(ResolveError::BadSigningKey(_))),
        },
        DocCase {
            name: "unsupported verification method type",
            requested: SQUID,
            body: || {
                legacy_document(
                    "JsonWebKey2020",
                    &format!("z{}", bs58::encode(sec1(&signer(5))).into_string()),
                )
            },
            expect: |r| matches!(r, Err(ResolveError::MissingSigningKey)),
        },
        DocCase {
            name: "document has only a non-atproto method",
            requested: SQUID,
            body: || {
                body(serde_json::json!({
                    "id": SQUID,
                    "verificationMethod": [{
                        "id": format!("{SQUID}#extra"),
                        "type": "Multikey",
                        "controller": SQUID,
                        "publicKeyMultibase": knot_types::crypto::multikey(0xe7, &sec1(&signer(3)))
                    }],
                    "service": [{
                        "id": "#atproto_pds",
                        "type": "AtprotoPersonalDataServer",
                        "serviceEndpoint": "https://pds.oyster.cafe"
                    }]
                }))
            },
            expect: |r| matches!(r, Err(ResolveError::MissingSigningKey)),
        },
        DocCase {
            name: "knot repo did:plc isn't resolvable as an account",
            requested: "did:plc:anemone",
            body: || {
                did_doc(DocSpec {
                    id: "did:plc:anemone",
                    signing: &signer(5),
                    handle: "nel.pet",
                    pds: "https://knot.oyster.cafe/repo/anemone",
                    method: MethodKind::None,
                })
            },
            expect: |r| matches!(r, Err(ResolveError::MissingSigningKey)),
        },
    ];

    #[test]
    fn identity_from_document_rejects_every_underspecified_document() {
        DOC_CASES.iter().for_each(|case| {
            let result = identity_from_document(&did(case.requested), &(case.body)());
            assert!(
                (case.expect)(&result),
                "case {:?} got {result:?}",
                case.name
            );
        });
    }
}
