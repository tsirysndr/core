use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use knot_runtime::{Entropy, PublicKeyBytes, Signer};
use knot_types::{ActorId, KnotId, KnotServiceUrl, OwnerDid, RepoDid, RepoRkey};
use serde::Serialize;
use sha2::{Digest, Sha256};

const PLC_OP_TYPE: &str = "plc_operation";
const ATPROTO_METHOD: &str = "atproto";
const KNOT_HOME_SERVICE: &str = "tangled_knot";
const KNOT_HOME_TYPE: &str = "TangledKnot";
const DID_PLC_PREFIX: &str = "did:plc:";
const DID_SUFFIX_LEN: usize = 24;
const MINT_NONCE_LEN: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("plc operation couldn't be encoded: {0}")]
    Encode(String),
    #[error("derived did:plc isn't valid DID: {0}")]
    Did(#[from] knot_types::ParseError),
}

#[derive(Serialize)]
struct PlcService {
    r#type: &'static str,
    endpoint: String,
}

#[derive(Serialize)]
struct PlcOperation {
    #[serde(rename = "type")]
    op_type: &'static str,
    #[serde(rename = "rotationKeys")]
    rotation_keys: Vec<String>,
    #[serde(rename = "verificationMethods")]
    verification_methods: BTreeMap<&'static str, String>,
    #[serde(rename = "alsoKnownAs")]
    also_known_as: Vec<String>,
    services: BTreeMap<&'static str, PlcService>,
    prev: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sig: Option<String>,
}

pub struct PreparedRepoDid {
    pub did: RepoDid,
    operation_json: Vec<u8>,
}

impl PreparedRepoDid {
    pub fn operation_json(&self) -> &[u8] {
        &self.operation_json
    }
}

fn did_key(public: &PublicKeyBytes) -> String {
    format!("did:key:{}", multikey_secp256k1(public))
}

fn multikey_secp256k1(public: &PublicKeyBytes) -> ActorId {
    ActorId::from_secp256k1(public.as_bytes())
}

fn encode_cbor(operation: &PlcOperation) -> Result<Vec<u8>, IdentityError> {
    serde_ipld_dagcbor::to_vec(operation).map_err(|error| IdentityError::Encode(error.to_string()))
}

pub struct MintNonce([u8; MINT_NONCE_LEN]);

impl MintNonce {
    pub fn mint(entropy: &dyn Entropy, owner: &OwnerDid, rkey: &RepoRkey) -> Self {
        let mut bytes = [0u8; MINT_NONCE_LEN];
        entropy.derive(mint_label(owner, rkey)).fill(&mut bytes);
        Self(bytes)
    }

    fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

fn mint_label(owner: &OwnerDid, rkey: &RepoRkey) -> u64 {
    [owner.as_str(), rkey.as_str()]
        .iter()
        .flat_map(|part| part.bytes().chain(std::iter::once(0u8)))
        .fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

pub fn prepare_repo_did(
    signer: &dyn Signer,
    knot_service_url: &KnotServiceUrl,
    mint_nonce: &MintNonce,
) -> Result<PreparedRepoDid, IdentityError> {
    let tag = base32::encode(
        base32::Alphabet::Rfc4648 { padding: false },
        mint_nonce.as_bytes(),
    )
    .to_lowercase();
    let base = knot_service_url.as_str();
    let mut operation = PlcOperation {
        op_type: PLC_OP_TYPE,
        rotation_keys: vec![did_key(&signer.public_key())],
        verification_methods: BTreeMap::new(),
        also_known_as: Vec::new(),
        services: BTreeMap::from([(
            KNOT_HOME_SERVICE,
            PlcService {
                r#type: KNOT_HOME_TYPE,
                endpoint: format!("{base}/repo/{tag}"),
            },
        )]),
        prev: None,
        sig: None,
    };

    let unsigned = encode_cbor(&operation)?;
    operation.sig = Some(URL_SAFE_NO_PAD.encode(signer.sign(&unsigned).as_bytes()));

    let signed = encode_cbor(&operation)?;
    let did = derive_did_plc(&signed)?;
    let operation_json =
        serde_json::to_vec(&operation).map_err(|error| IdentityError::Encode(error.to_string()))?;
    Ok(PreparedRepoDid {
        did,
        operation_json,
    })
}

fn derive_did_plc(signed_cbor: &[u8]) -> Result<RepoDid, IdentityError> {
    let digest = Sha256::digest(signed_cbor);
    let encoded =
        base32::encode(base32::Alphabet::Rfc4648 { padding: false }, &digest).to_lowercase();
    let suffix: String = encoded.chars().take(DID_SUFFIX_LEN).collect();
    Ok(RepoDid::new(format!("{DID_PLC_PREFIX}{suffix}"))?)
}

pub fn knot_did_document(
    knot: &KnotId,
    signing_key: &PublicKeyBytes,
    service_url: &KnotServiceUrl,
) -> serde_json::Value {
    did_web_document(knot, signing_key, service_url)
}

fn did_web_document(
    id: &KnotId,
    signing_key: &PublicKeyBytes,
    service_url: &KnotServiceUrl,
) -> serde_json::Value {
    serde_json::json!({
        "@context": [
            "https://www.w3.org/ns/did/v1",
            "https://w3id.org/security/multikey/v1",
            "https://w3id.org/security/suites/secp256k1-2019/v1"
        ],
        "id": id,
        "verificationMethod": [{
            "id": format!("{id}#{ATPROTO_METHOD}"),
            "type": "Multikey",
            "controller": id,
            "publicKeyMultibase": multikey_secp256k1(signing_key)
        }],
        "service": [{
            "id": format!("#{KNOT_HOME_SERVICE}"),
            "type": KNOT_HOME_TYPE,
            "serviceEndpoint": service_url
        }]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use knot_runtime::verify;

    #[test]
    fn did_derivation_is_deterministic_and_nonce_sensitive() {
        let key = runtime_signer(2);
        let first = prepare_repo_did(
            &key,
            &KnotServiceUrl::new("https://nel.pet").unwrap(),
            &repo_nonce(101),
        )
        .unwrap();
        let again = prepare_repo_did(
            &key,
            &KnotServiceUrl::new("https://nel.pet").unwrap(),
            &repo_nonce(101),
        )
        .unwrap();
        assert_eq!(first.did, again.did);
        assert_eq!(first.operation_json(), again.operation_json());

        let slashed = prepare_repo_did(
            &key,
            &KnotServiceUrl::new("https://nel.pet/").unwrap(),
            &repo_nonce(101),
        )
        .unwrap();
        assert_eq!(
            first.did, slashed.did,
            "a trailing slash on the knot url changes neither the endpoint nor the did"
        );

        let other_nonce = prepare_repo_did(
            &key,
            &KnotServiceUrl::new("https://nel.pet").unwrap(),
            &repo_nonce(102),
        )
        .unwrap();
        assert_ne!(
            first.did, other_nonce.did,
            "shared knot key no longer distinguishes repos; mint nonce must"
        );
    }

    #[test]
    fn the_derived_did_plc_is_pinned_and_well_formed() {
        let prepared = prepare_repo_did(
            &runtime_signer(1),
            &KnotServiceUrl::new("https://knot.oyster.cafe").unwrap(),
            &repo_nonce(104),
        )
        .unwrap();
        let did = prepared.did.as_str();
        assert_eq!(
            did, "did:plc:obafda42ebtgg5thl7bzyjso",
            "any change to this value means did:plc derivation no longer matches the PLC directory"
        );
        let suffix = did.strip_prefix(DID_PLC_PREFIX).unwrap();
        assert_eq!(suffix.len(), DID_SUFFIX_LEN);
        assert!(
            suffix
                .chars()
                .all(|c| c.is_ascii_lowercase() || ('2'..='7').contains(&c)),
            "the did:plc suffix is lowercase base32"
        );
    }

    #[test]
    fn the_operation_signature_verifies_against_the_repo_key_over_the_unsigned_cbor() {
        let key = runtime_signer(5);
        let prepared = prepare_repo_did(
            &key,
            &KnotServiceUrl::new("https://nel.pet").unwrap(),
            &repo_nonce(106),
        )
        .unwrap();
        let operation: serde_json::Value =
            serde_json::from_slice(prepared.operation_json()).unwrap();

        let signature_b64 = operation["sig"].as_str().unwrap();
        let signature =
            knot_runtime::Signature::from_bytes(URL_SAFE_NO_PAD.decode(signature_b64).unwrap());

        let mut unsigned = operation.clone();
        unsigned.as_object_mut().unwrap().remove("sig");
        let unsigned_cbor = serde_ipld_dagcbor::to_vec(&unsigned).unwrap();

        assert!(
            verify(&key.public_key(), &unsigned_cbor, &signature),
            "genesis op is self-signed by repo rotation key over its unsigned dag-cbor"
        );
    }

    #[test]
    fn the_genesis_op_marks_the_home_knot_and_has_no_signing_key() {
        let key = runtime_signer(6);
        let prepared = prepare_repo_did(
            &key,
            &KnotServiceUrl::new("https://knot.oyster.cafe").unwrap(),
            &repo_nonce(107),
        )
        .unwrap();
        let operation: serde_json::Value =
            serde_json::from_slice(prepared.operation_json()).unwrap();

        assert_eq!(operation["type"], "plc_operation");
        assert_eq!(operation["prev"], serde_json::Value::Null);
        assert_eq!(operation["alsoKnownAs"], serde_json::json!([]));
        assert!(
            operation["services"][KNOT_HOME_SERVICE]["endpoint"]
                .as_str()
                .unwrap()
                .starts_with("https://knot.oyster.cafe/repo/")
        );
        assert_eq!(
            operation["services"][KNOT_HOME_SERVICE]["type"],
            KNOT_HOME_TYPE
        );
        assert!(operation["services"]["atproto_pds"].is_null());
        assert_eq!(
            operation["verificationMethods"],
            serde_json::json!({}),
            "repos have no atproto signing key"
        );
        let expected_key = did_key(&key.public_key());
        assert_eq!(operation["rotationKeys"][0], expected_key);
        assert_eq!(operation["rotationKeys"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn the_knot_document_declares_its_signing_key_and_tangled_knot_service() {
        let key = runtime_signer(7);
        let knot = KnotId::new("did:web:knot.oyster.cafe").unwrap();
        let document = knot_did_document(
            &knot,
            &key.public_key(),
            &KnotServiceUrl::new("https://knot.oyster.cafe").unwrap(),
        );

        assert_eq!(
            document["verificationMethod"][0]["id"], "did:web:knot.oyster.cafe#atproto",
            "knot publishes its signing key under the atproto method"
        );
        assert_eq!(
            document["verificationMethod"][0]["publicKeyMultibase"],
            serde_json::json!(multikey_secp256k1(&key.public_key())),
            "published verification method is the knot's own signing key"
        );
        assert_eq!(
            document["service"][0],
            serde_json::json!({
                "id": "#tangled_knot",
                "type": "TangledKnot",
                "serviceEndpoint": "https://knot.oyster.cafe"
            }),
            "knot self-declares its tangled_knot service at the knot root"
        );
    }
}
