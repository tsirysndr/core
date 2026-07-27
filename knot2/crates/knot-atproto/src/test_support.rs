use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use bytes::Bytes;
use http::StatusCode;
use k256::ecdsa::{Signature, SigningKey, signature::Signer};
use knot_runtime::{HttpResponse, K256Signer, ManualClock, SeededEntropy, UnixMicros};
use knot_types::crypto::{KeyCodec, PublicKey as CryptoKey};
use knot_types::{AccountDid, KnotId, Nsid, OwnerDid, RepoDid, RepoRkey};
use serde_json::Value;
use url::Url;

use crate::{MintNonce, ServiceJwt};

pub(crate) const SQUID: &str = "did:plc:squid";
pub(crate) const LIMPET: &str = "did:plc:limpet";
pub(crate) const KNOT: &str = "did:web:nel.pet";
pub(crate) const METHOD: &str = "sh.tangled.knot.addMember";

pub(crate) fn did(value: &str) -> AccountDid {
    AccountDid::new(value).unwrap()
}

pub(crate) fn repo_did(value: &str) -> RepoDid {
    RepoDid::new(value).unwrap()
}

pub(crate) fn owner_did(value: &str) -> OwnerDid {
    OwnerDid::new(value).unwrap()
}

pub(crate) fn knot_did(value: &str) -> KnotId {
    KnotId::new(value).unwrap()
}

pub(crate) fn handle(value: &str) -> knot_types::Handle {
    knot_types::Handle::new_owned(value).unwrap()
}

pub(crate) fn member_method() -> Nsid {
    Nsid::new_owned(METHOD).unwrap()
}

pub(crate) fn plc() -> crate::PlcDirectory {
    crate::PlcDirectory::new(Url::parse("https://plc.directory/").unwrap()).unwrap()
}

pub(crate) fn clock() -> ManualClock {
    ManualClock::new(UnixMicros::new(1_000_000_000))
}

pub(crate) fn signer(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32].into()).unwrap()
}

pub(crate) fn sec1(signing: &SigningKey) -> Vec<u8> {
    signing
        .verifying_key()
        .to_encoded_point(true)
        .as_bytes()
        .to_vec()
}

pub(crate) fn k256_public(signing: &SigningKey) -> CryptoKey<'static> {
    CryptoKey {
        codec: KeyCodec::Secp256k1,
        bytes: Cow::Owned(sec1(signing)),
    }
}

pub(crate) enum MethodKind {
    Multikey,
    LegacyK256,
    LegacyP256,
    None,
}

pub(crate) struct DocSpec<'a> {
    pub id: &'a str,
    pub signing: &'a SigningKey,
    pub handle: &'a str,
    pub pds: &'a str,
    pub method: MethodKind,
}

pub(crate) fn did_doc(spec: DocSpec) -> Bytes {
    let raw = sec1(spec.signing);
    let legacy = || format!("z{}", bs58::encode(&raw).into_string());
    let method_entry: Option<(&'static str, String)> = match spec.method {
        MethodKind::Multikey => Some(("Multikey", knot_types::crypto::multikey(0xe7, &raw))),
        MethodKind::LegacyK256 => Some(("EcdsaSecp256k1VerificationKey2019", legacy())),
        MethodKind::LegacyP256 => Some(("EcdsaSecp256r1VerificationKey2019", legacy())),
        MethodKind::None => None,
    };
    let verification_method: Value = match method_entry {
        Some((kind, multibase)) => serde_json::json!([{
            "id": format!("{}#atproto", spec.id),
            "type": kind,
            "controller": spec.id,
            "publicKeyMultibase": multibase,
        }]),
        None => serde_json::json!([]),
    };
    let body = serde_json::json!({
        "id": spec.id,
        "alsoKnownAs": [format!("at://{}", spec.handle)],
        "verificationMethod": verification_method,
        "service": [{
            "id": "#atproto_pds",
            "type": "AtprotoPersonalDataServer",
            "serviceEndpoint": spec.pds,
        }]
    });
    Bytes::from(serde_json::to_vec(&body).unwrap())
}

pub(crate) fn ssh_line(algo: &str, material: &[u8], comment: &str) -> String {
    let ssh_string = |bytes: &[u8]| [&(bytes.len() as u32).to_be_bytes()[..], bytes].concat();
    let blob = [ssh_string(algo.as_bytes()), ssh_string(material)].concat();
    format!("{algo} {} {comment}", STANDARD.encode(blob))
}

pub(crate) fn list_body(lines: &[String], cursor: Option<&str>) -> Bytes {
    let records: Vec<_> = lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            serde_json::json!({
                "uri": format!("at://{SQUID}/sh.tangled.publicKey/{index}"),
                "value": { "$type": "sh.tangled.publicKey", "key": line, "name": "k", "createdAt": "2026-06-08T00:00:00Z" }
            })
        })
        .collect();
    let cursor_field: Value = cursor.map_or(Value::Null, |value| serde_json::json!(value));
    let body = serde_json::json!({ "records": records, "cursor": cursor_field });
    Bytes::from(serde_json::to_vec(&body).unwrap())
}

pub(crate) fn ok(body: Bytes) -> HttpResponse {
    status(StatusCode::OK, body)
}

pub(crate) fn status(status: StatusCode, body: Bytes) -> HttpResponse {
    HttpResponse {
        status,
        headers: http::HeaderMap::new(),
        body,
    }
}

pub(crate) fn mint(signing: &SigningKey, claims: &Value) -> ServiceJwt {
    mint_with_header(signing, br#"{"alg":"ES256K","typ":"JWT"}"#, claims)
}

pub(crate) fn mint_with_header(signing: &SigningKey, header: &[u8], claims: &Value) -> ServiceJwt {
    let header_b64 = URL_SAFE_NO_PAD.encode(header);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap());
    let signing_input = format!("{header_b64}.{payload}");
    let signature: Signature = signing.sign(signing_input.as_bytes());
    ServiceJwt::new(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
    .expect("minted token is structurally a JWT")
}

pub(crate) fn runtime_signer(seed: u64) -> K256Signer {
    K256Signer::generate(&SeededEntropy::new(seed))
}

pub(crate) fn entropy(seed: u64) -> SeededEntropy {
    SeededEntropy::new(seed)
}

pub(crate) fn repo_nonce(seed: u64) -> MintNonce {
    MintNonce::mint(
        &entropy(seed),
        &owner_did("did:plc:nel"),
        &RepoRkey::new("anemone").unwrap(),
    )
}

pub(crate) fn member_pointer() -> knot_lexicons::sh_tangled::knot::member::Member {
    knot_lexicons::sh_tangled::knot::member::Member {
        created_at: knot_types::Datetime::raw_str("2026-06-11T00:00:00Z"),
        domain: "knot.nel.pet".into(),
        subject: knot_types::Did::new_owned("did:plc:lyna").unwrap(),
        extra_data: None,
    }
}

pub(crate) type UrlLog = Arc<Mutex<Vec<Url>>>;

pub(crate) fn recorder() -> (UrlLog, UrlLog) {
    let urls: UrlLog = Arc::new(Mutex::new(Vec::new()));
    (urls.clone(), urls)
}
