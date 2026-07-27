use knot_types::{AccountDid, AtUri, Cid, Collection, Nsid, Rkey, ServiceDid};
use serde::{Deserialize, Serialize};

use crate::AtprotoError;
use crate::resolve::PdsEndpoint;

pub fn put_record_method() -> Nsid {
    Nsid::new_static("com.atproto.repo.putRecord").expect("literal method nsid parses")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerReceipt {
    pub uri: AtUri,
    pub cid: Cid,
}

pub(crate) fn pds_service_did(pds: &PdsEndpoint) -> Result<ServiceDid, AtprotoError> {
    let bad = || AtprotoError::BadPdsEndpoint {
        pds: pds.url().as_str().to_string(),
    };
    let host = pds
        .url()
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or_else(bad)?;
    let authority = pds
        .url()
        .port()
        .map_or_else(|| host.to_string(), |port| format!("{host}%3A{port}"));
    let msid = std::iter::once(authority)
        .chain(
            pds.url()
                .path_segments()
                .into_iter()
                .flatten()
                .filter(|segment| !segment.is_empty())
                .map(str::to_string),
        )
        .collect::<Vec<_>>()
        .join(":");
    ServiceDid::new(format!("did:web:{msid}")).map_err(|_| bad())
}

#[derive(Serialize)]
struct PutRecordInput<'a, R: Serialize> {
    repo: &'a AccountDid,
    collection: &'static str,
    rkey: &'a Rkey,
    record: &'a R,
}

pub(crate) fn put_record_body<R: Collection + Serialize>(
    subject: &AccountDid,
    rkey: &Rkey,
    record: &R,
) -> Result<Vec<u8>, AtprotoError> {
    serde_json::to_vec(&PutRecordInput {
        repo: subject,
        collection: R::NSID,
        rkey,
        record,
    })
    .map_err(|error| AtprotoError::PointerEncode(error.to_string()))
}

#[derive(Deserialize)]
struct PutRecordOutput {
    uri: String,
    cid: String,
}

pub(crate) fn receipt_from_response(body: &[u8]) -> Result<PointerReceipt, AtprotoError> {
    let output: PutRecordOutput = serde_json::from_slice(body)
        .map_err(|error| AtprotoError::MalformedReceipt(error.to_string()))?;
    let uri = AtUri::new_owned(&output.uri)
        .map_err(|error| AtprotoError::MalformedReceipt(error.to_string()))?;
    let cid = Cid::new_owned(output.cid.as_bytes())
        .map_err(|error| AtprotoError::MalformedReceipt(error.to_string()))
        .and_then(|cid: Cid| {
            cid.is_valid().then_some(cid).ok_or_else(|| {
                AtprotoError::MalformedReceipt(format!("cid {:?} doesn't parse", output.cid))
            })
        })?;
    Ok(PointerReceipt { uri, cid })
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    fn service_did_of(value: &str) -> Result<ServiceDid, AtprotoError> {
        PdsEndpoint::new(Url::parse(value).unwrap())
            .map_err(|_| AtprotoError::BadPdsEndpoint {
                pds: value.to_string(),
            })
            .and_then(|pds| pds_service_did(&pds))
    }

    struct PdsCase {
        url: &'static str,
        expect: fn(&Result<ServiceDid, AtprotoError>) -> bool,
    }

    const PDS_CASES: &[PdsCase] = &[
        PdsCase {
            url: "https://pds.oyster.cafe",
            expect: |r| matches!(r, Ok(did) if did.as_str() == "did:web:pds.oyster.cafe"),
        },
        PdsCase {
            url: "https://pds.oyster.cafe:8443",
            expect: |r| matches!(r, Ok(did) if did.as_str() == "did:web:pds.oyster.cafe%3A8443"),
        },
        PdsCase {
            url: "https://shared.host/account-pds",
            expect: |r| matches!(r, Ok(did) if did.as_str() == "did:web:shared.host:account-pds"),
        },
        PdsCase {
            url: "unix:/run/pds.sock",
            expect: |r| matches!(r, Err(AtprotoError::BadPdsEndpoint { .. })),
        },
    ];

    #[test]
    fn pds_service_did_maps_each_endpoint_shape_to_its_web_did() {
        PDS_CASES.iter().for_each(|case| {
            let result = service_did_of(case.url);
            assert!((case.expect)(&result), "case {:?} got {result:?}", case.url);
        });
    }

    #[test]
    fn a_receipt_round_trips_its_uri_and_cid() {
        let body = serde_json::json!({
            "uri": "at://did:plc:squid/sh.tangled.knot.member/3jzfcijpj2z2a",
            "cid": "bafyreidfayvfuwqa7qlnopdjiqrxzs6blmoeu4rujcjtnci5beludirz2a"
        });
        let receipt = receipt_from_response(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(
            receipt.uri.as_str(),
            "at://did:plc:squid/sh.tangled.knot.member/3jzfcijpj2z2a"
        );
        assert_eq!(
            receipt.cid.as_str(),
            "bafyreidfayvfuwqa7qlnopdjiqrxzs6blmoeu4rujcjtnci5beludirz2a"
        );
    }

    #[test]
    fn a_garbage_receipt_is_a_typed_error() {
        assert!(matches!(
            receipt_from_response(b"not json"),
            Err(AtprotoError::MalformedReceipt(_))
        ));
        let bad_uri = serde_json::json!({ "uri": "http://nope", "cid": "bafyreidfayvfuwqa7qlnopdjiqrxzs6blmoeu4rujcjtnci5beludirz2a" });
        assert!(matches!(
            receipt_from_response(&serde_json::to_vec(&bad_uri).unwrap()),
            Err(AtprotoError::MalformedReceipt(_))
        ));
        let bad_cid = serde_json::json!({ "uri": "at://did:plc:squid/sh.tangled.knot.member/3jzfcijpj2z2a", "cid": "not-a-cid" });
        assert!(matches!(
            receipt_from_response(&serde_json::to_vec(&bad_cid).unwrap()),
            Err(AtprotoError::MalformedReceipt(_))
        ));
    }
}
