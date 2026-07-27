use k256::ecdsa::signature::Verifier as _;
use k256::ecdsa::{Signature as K256Signature, VerifyingKey};
use knot_runtime::Signature;
use knot_types::crypto::PublicKey;
use knot_types::{ActorId, ChangeId, CobId, KnotId, Oid, RepoDid, TypeName, UnixSeconds};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::PayloadError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CobHome {
    Repo(RepoDid),
    Knot(KnotId),
}

impl CobHome {
    pub fn as_str(&self) -> &str {
        match self {
            CobHome::Repo(did) => did.as_str(),
            CobHome::Knot(knot) => knot.as_str(),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            CobHome::Repo(_) => "repo",
            CobHome::Knot(_) => "knot",
        }
    }
}

impl From<&RepoDid> for CobHome {
    fn from(did: &RepoDid) -> Self {
        CobHome::Repo(did.clone())
    }
}

impl From<&KnotId> for CobHome {
    fn from(knot: &KnotId) -> Self {
        CobHome::Knot(knot.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payload(Vec<u8>);

impl Payload {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

pub trait ChangePayload: Serialize + DeserializeOwned + Sized {
    const TYPE: &'static str;

    fn type_name() -> TypeName {
        TypeName::new(Self::TYPE).expect("ChangePayload::TYPE must be valid nsid")
    }

    fn encode(&self) -> Result<Vec<u8>, PayloadError> {
        serde_ipld_dagcbor::to_vec(self).map_err(|error| PayloadError::Encode(error.to_string()))
    }

    fn decode(bytes: &[u8]) -> Result<Self, PayloadError> {
        serde_ipld_dagcbor::from_slice(bytes)
            .map_err(|error| PayloadError::Decode(error.to_string()))
    }
}

const SIGNING_CONTEXT: &str = "sh.tangled.knot.cob.change.v1";

#[derive(Serialize)]
struct SignedChange<'a> {
    context: &'static str,
    #[serde(rename = "homeKind")]
    home_kind: &'static str,
    home: &'a str,
    revision: Oid,
    parents: &'a [ChangeId],
    #[serde(rename = "typeName")]
    type_name: &'a TypeName,
    author: &'a ActorId,
    timestamp: UnixSeconds,
    #[serde(skip_serializing_if = "Option::is_none")]
    object: Option<CobId>,
}

pub(crate) fn signing_bytes(
    home: &CobHome,
    revision: Oid,
    parents: &[ChangeId],
    type_name: &TypeName,
    author: &ActorId,
    timestamp: UnixSeconds,
    object: Option<CobId>,
) -> Vec<u8> {
    let view = SignedChange {
        context: SIGNING_CONTEXT,
        home_kind: home.kind(),
        home: home.as_str(),
        revision,
        parents,
        type_name,
        author,
        timestamp,
        object,
    };
    serde_ipld_dagcbor::to_vec(&view).expect("change signing view always encodes")
}

pub(crate) fn object_binding(parents: &[ChangeId], object: Option<CobId>) -> Option<CobId> {
    if parents.is_empty() { None } else { object }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn verify_signature(
    home: &CobHome,
    revision: Oid,
    parents: &[ChangeId],
    type_name: &TypeName,
    author: &ActorId,
    timestamp: UnixSeconds,
    object: Option<CobId>,
    signature: &[u8],
) -> bool {
    let Ok(public) = PublicKey::decode(author.as_str()) else {
        return false;
    };
    let Ok(verifying) = public.to_k256() else {
        return false;
    };
    let Ok(signature) = K256Signature::from_slice(signature) else {
        return false;
    };
    let message = signing_bytes(
        home,
        revision,
        parents,
        type_name,
        author,
        timestamp,
        object_binding(parents, object),
    );
    VerifyingKey::from(&verifying)
        .verify(&message, &signature)
        .is_ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub id: ChangeId,
    pub revision: Oid,
    pub parents: Vec<ChangeId>,
    pub type_name: TypeName,
    pub author: ActorId,
    pub signature: Signature,
    pub payload: Payload,
    pub timestamp: UnixSeconds,
}

impl Change {
    pub fn payload(&self) -> &[u8] {
        self.payload.as_bytes()
    }

    pub fn sort_key(&self) -> (UnixSeconds, ChangeId) {
        (self.timestamp, self.id)
    }

    pub fn verify(&self, home: &CobHome, expected_author: &ActorId, object: Option<CobId>) -> bool {
        &self.author == expected_author
            && verify_signature(
                home,
                self.revision,
                &self.parents,
                &self.type_name,
                &self.author,
                self.timestamp,
                object,
                self.signature.as_bytes(),
            )
    }
}

#[cfg(test)]
mod tests {
    use knot_runtime::{K256Signer, SeededEntropy, Signer};

    use super::*;

    fn type_name() -> TypeName {
        TypeName::new("sh.tangled.test.tag").unwrap()
    }

    fn cob_home() -> CobHome {
        CobHome::from(&RepoDid::new("did:plc:squid").unwrap())
    }

    fn signed_change(
        signer: &K256Signer,
        revision: Oid,
        parents: Vec<ChangeId>,
        timestamp: i64,
    ) -> Change {
        let author = ActorId::from_secp256k1(signer.public_key().as_bytes());
        let timestamp = UnixSeconds::new(timestamp);
        let bytes = signing_bytes(
            &cob_home(),
            revision,
            &parents,
            &type_name(),
            &author,
            timestamp,
            object_binding(&parents, None),
        );
        Change {
            id: ChangeId::new(Oid::null()),
            revision,
            parents,
            type_name: type_name(),
            author,
            signature: signer.sign(&bytes),
            payload: Payload::new(Vec::new()),
            timestamp,
        }
    }

    #[test]
    fn signing_bytes_stay_byte_stable() {
        let author = {
            let mut compressed = [0u8; 33];
            compressed[0] = 0x02;
            compressed[1] = 0x09;
            ActorId::from_secp256k1(&compressed)
        };
        let parents = vec![ChangeId::new(
            Oid::from_hex("2222222222222222222222222222222222222222").unwrap(),
        )];
        let object = Some(CobId::new(
            Oid::from_hex("3333333333333333333333333333333333333333").unwrap(),
        ));
        let bytes = signing_bytes(
            &cob_home(),
            Oid::from_hex("1111111111111111111111111111111111111111").unwrap(),
            &parents,
            &type_name(),
            &author,
            UnixSeconds::new(1_700_000_000),
            object_binding(&parents, object),
        );
        assert_eq!(
            knot_types::lowercase_hex(&bytes),
            "a964686f6d656d6469643a706c633a737175696466617574686f7278317a513373684e317652664257527847397234564c3251796466474e675955715a5a385836743971535359774b636a644a50666f626a65637478283333333333333333333333333333333333333333333333333333333333333333333333333333333367636f6e74657874781d73682e74616e676c65642e6b6e6f742e636f622e6368616e67652e763167706172656e74738178283232323232323232323232323232323232323232323232323232323232323232323232323232323268686f6d654b696e64647265706f687265766973696f6e78283131313131313131313131313131313131313131313131313131313131313131313131313131313168747970654e616d657373682e74616e676c65642e746573742e7461676974696d657374616d701a6553f100"
        );
    }

    #[test]
    fn verify_binds_the_author() {
        let signer = K256Signer::generate(&SeededEntropy::new(13));
        let stranger = K256Signer::generate(&SeededEntropy::new(14));
        let revision = Oid::from_hex("6666666666666666666666666666666666666666").unwrap();
        let change = signed_change(&signer, revision, Vec::new(), 1);
        let stranger_actor = ActorId::from_secp256k1(stranger.public_key().as_bytes());
        assert!(change.verify(&cob_home(), &change.author, None));
        assert!(
            !change.verify(&cob_home(), &stranger_actor, None),
            "a valid signature under an unexpected expected-author is refused"
        );
        let foreign = Change {
            author: stranger_actor,
            ..change
        };
        assert!(
            !foreign.verify(&cob_home(), &foreign.author, None),
            "an author swapped to a stranger fails its own signature check"
        );
    }

    #[test]
    fn verify_rejects_a_tampered_transplanted_or_rehomed_change() {
        let signer = K256Signer::generate(&SeededEntropy::new(10));
        let revision = Oid::from_hex("1111111111111111111111111111111111111111").unwrap();
        let genuine = signed_change(&signer, revision, Vec::new(), 1);
        assert!(
            genuine.verify(&cob_home(), &genuine.author, None),
            "genuine root change verifies"
        );

        let mutators: Vec<fn(Change) -> Change> = vec![
            |change| {
                let mut bytes = change.signature.as_bytes().to_vec();
                bytes[0] ^= 0xff;
                Change {
                    signature: Signature::from_bytes(bytes),
                    ..change
                }
            },
            |change| Change {
                parents: vec![ChangeId::new(
                    Oid::from_hex("3333333333333333333333333333333333333333").unwrap(),
                )],
                ..change
            },
            |change| Change {
                timestamp: UnixSeconds::new(9_999_999),
                ..change
            },
        ];
        mutators.into_iter().for_each(|mutate| {
            let broken = mutate(genuine.clone());
            assert!(
                !broken.verify(&cob_home(), &broken.author, None),
                "a tampered or transplanted change fails verification"
            );
        });

        let other_home = CobHome::from(&RepoDid::new("did:plc:limpet").unwrap());
        assert!(
            !genuine.verify(&other_home, &genuine.author, None),
            "change signed for one repo mustn't verify under another repo's home"
        );

        let author = ActorId::from_secp256k1(signer.public_key().as_bytes());
        let parent =
            ChangeId::new(Oid::from_hex("5555555555555555555555555555555555555555").unwrap());
        let home = CobId::new(Oid::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap());
        let elsewhere =
            CobId::new(Oid::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap());
        let timestamp = UnixSeconds::new(1);
        let bytes = signing_bytes(
            &cob_home(),
            revision,
            &[parent],
            &type_name(),
            &author,
            timestamp,
            object_binding(&[parent], Some(home)),
        );
        let bound = Change {
            id: ChangeId::new(Oid::null()),
            revision,
            parents: vec![parent],
            type_name: type_name(),
            author,
            signature: signer.sign(&bytes),
            payload: Payload::new(Vec::new()),
            timestamp,
        };
        assert!(bound.verify(&cob_home(), &bound.author, Some(home)));
        assert!(
            !bound.verify(&cob_home(), &bound.author, Some(elsewhere)),
            "a non-root change is bound to its object"
        );
        assert!(!bound.verify(&cob_home(), &bound.author, None));
    }
}
