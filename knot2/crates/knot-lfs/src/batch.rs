use serde::{Deserialize, Deserializer, Serialize, Serializer};
use url::Url;

use knot_types::{HttpStatus, RefName};

use crate::{ClaimedSize, LfsOid};

pub const BATCH_MEDIA_TYPE: &str = "application/vnd.git-lfs+json";
pub const HASH_ALGO: &str = "sha256";
pub const BASIC_TRANSFER: &str = "basic";
pub const MAX_BATCH_OBJECTS: usize = 1000;

// Unknown adapters parse instead of failing the whole body,
// so that a client offering something we don't serve
// will at least get a 422 that shows a mismatch not a serde error!
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferAdapter {
    Basic,
    Other(String),
}

impl TransferAdapter {
    pub fn is_basic(&self) -> bool {
        matches!(self, Self::Basic)
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Basic => BASIC_TRANSFER,
            Self::Other(value) => value,
        }
    }
}

impl Serialize for TransferAdapter {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for TransferAdapter {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(if raw == BASIC_TRANSFER {
            Self::Basic
        } else {
            Self::Other(raw)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HashAlgo {
    Sha256,
    Other(String),
}

impl HashAlgo {
    pub fn is_sha256(&self) -> bool {
        matches!(self, Self::Sha256)
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Sha256 => HASH_ALGO,
            Self::Other(value) => value,
        }
    }
}

impl Serialize for HashAlgo {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for HashAlgo {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(if raw == HASH_ALGO {
            Self::Sha256
        } else {
            Self::Other(raw)
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BatchOperation {
    Download,
    Upload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchRef {
    pub name: RefName,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchObject {
    pub oid: LfsOid,
    pub size: ClaimedSize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchRequest {
    pub operation: BatchOperation,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transfers: Vec<TransferAdapter>,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<BatchRef>,
    pub objects: Vec<BatchObject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash_algo: Option<HashAlgo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchAction {
    pub href: Url,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchActions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download: Option<BatchAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload: Option<BatchAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchObjectError {
    pub code: HttpStatus,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchResponseObject {
    pub oid: LfsOid,
    pub size: ClaimedSize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authenticated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actions: Option<BatchActions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<BatchObjectError>,
}

fn basic_transfer() -> TransferAdapter {
    TransferAdapter::Basic
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchResponse {
    #[serde(default = "basic_transfer")]
    pub transfer: TransferAdapter,
    pub objects: Vec<BatchResponseObject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash_algo: Option<HashAlgo>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const OID: &str = "6c17f2007cbe934aee6e309b28b2fba3c119d98be6ea4156da3aa3173456ad16";

    #[test]
    fn a_client_batch_request_round_trips() {
        let body = format!(
            r#"{{"operation":"download","transfers":["basic","ssh"],"ref":{{"name":"refs/heads/main"}},"objects":[{{"oid":"{OID}","size":42}}],"hash_algo":"sha256"}}"#
        );
        let request: BatchRequest = serde_json::from_str(&body).unwrap();
        assert_eq!(request.operation, BatchOperation::Download);
        assert_eq!(request.objects[0].oid.as_str(), OID);
        assert_eq!(request.objects[0].size, ClaimedSize::new(42));
        assert_eq!(
            request.reference.as_ref().unwrap().name.as_str(),
            "refs/heads/main"
        );
    }

    #[test]
    fn a_hostile_oid_fails_the_whole_parse() {
        let body = r#"{"operation":"download","objects":[{"oid":"../../etc/passwd","size":1}]}"#;
        assert!(serde_json::from_str::<BatchRequest>(body).is_err());
    }

    #[test]
    fn a_response_serializes_the_lfs_shape() {
        let response = BatchResponse {
            transfer: TransferAdapter::Basic,
            objects: vec![BatchResponseObject {
                oid: LfsOid::new(OID).unwrap(),
                size: ClaimedSize::new(42),
                authenticated: Some(true),
                actions: Some(BatchActions {
                    download: Some(BatchAction {
                        href: Url::parse(&format!(
                            "https://nel.pet/did:plc:squid/media/info/lfs/objects/{OID}"
                        ))
                        .unwrap(),
                    }),
                    upload: None,
                }),
                error: None,
            }],
            hash_algo: Some(HashAlgo::Sha256),
        };
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["transfer"], "basic");
        assert_eq!(json["objects"][0]["oid"], OID);
        assert_eq!(json["objects"][0]["authenticated"], true);
        assert!(
            json["objects"][0]["actions"]["download"]["href"]
                .as_str()
                .unwrap()
                .ends_with(OID)
        );
        assert_eq!(json["objects"][0].get("error"), None);
    }
}
