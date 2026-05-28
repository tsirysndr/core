use std::sync::Arc;
use std::time::Duration;

use bobbin_runtime::{HttpRequest, HttpResponseHead, HttpTransport, NetworkError, ReqwestHttp};
use bobbin_types::record::RecordBody;
use bytes::{Bytes, BytesMut};
use cid::Cid as IpldCid;
use futures::TryStreamExt;
use http::{HeaderMap, StatusCode};
use jacquard_common::BosStr;
use jacquard_common::types::did::Did;
use jacquard_common::types::ident::AtIdentifier;
use jacquard_common::types::nsid::Nsid;
use jacquard_common::types::recordkey::Rkey;
use jacquard_common::types::string::{AtStrError, AtUri};
use serde::Deserialize;
use serde_json::value::RawValue;
use thiserror::Error;
use url::Url;

const USER_AGENT: &str = concat!("bobbin/", env!("CARGO_PKG_VERSION"));
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const GET_RECORD_PATH: &str = "xrpc/com.atproto.repo.getRecord";
const RESOLVE_MINI_DOC_PATH: &str = "xrpc/com.bad-example.identity.resolveMiniDoc";
pub const MAX_BODY_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct SlingshotClient {
    http: Arc<dyn HttpTransport>,
    base: Url,
}

impl std::fmt::Debug for SlingshotClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlingshotClient")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Error)]
pub enum SlingshotError {
    #[error("invalid base url scheme: {0}")]
    BadScheme(String),
    #[error("network: {0}")]
    Network(#[from] NetworkError),
    #[error("http client build: {0}")]
    Build(String),
    #[error("record not found")]
    NotFound,
    #[error("upstream returned status {0}")]
    Upstream(StatusCode),
    #[error("response body exceeded {limit} bytes")]
    BodyTooLarge { limit: u64 },
    #[error("decode response: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("response missing required field: {0}")]
    MissingField(&'static str),
    #[error("invalid AT-URI in response: {0}")]
    InvalidAtUri(#[from] AtStrError),
    #[error("invalid CID in response: {0}")]
    InvalidCid(#[from] cid::Error),
    #[error("upstream returned uri {got}, expected {expected}")]
    UriMismatch { expected: String, got: String },
}

pub fn default_http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
}

impl SlingshotClient {
    pub fn new(base: Url, http: Arc<dyn HttpTransport>) -> Result<Self, SlingshotError> {
        match base.scheme() {
            "http" | "https" => {}
            other => return Err(SlingshotError::BadScheme(other.to_owned())),
        }
        let base = ensure_trailing_slash(base);
        Ok(Self { http, base })
    }

    pub fn with_default_http(base: Url) -> Result<Self, SlingshotError> {
        let client = default_http_client().map_err(|e| SlingshotError::Build(e.to_string()))?;
        Self::new(base, ReqwestHttp::shared(client))
    }

    pub async fn resolve_mini_doc<S>(
        &self,
        identifier: &AtIdentifier<S>,
    ) -> Result<Bytes, SlingshotError>
    where
        S: BosStr,
    {
        let mut url = self.base.join(RESOLVE_MINI_DOC_PATH).expect(
            "base url is hierarchical and RESOLVE_MINI_DOC_PATH is a literal relative path",
        );
        url.query_pairs_mut()
            .clear()
            .append_pair("identifier", identifier.as_str());

        let resp = self
            .http
            .execute(HttpRequest {
                url,
                headers: HeaderMap::new(),
            })
            .await?;
        match resp.status {
            StatusCode::OK => read_bounded(resp).await,
            StatusCode::NOT_FOUND => Err(SlingshotError::NotFound),
            other => Err(SlingshotError::Upstream(other)),
        }
    }

    pub async fn get_record<S>(
        &self,
        repo: &Did<S>,
        collection: &Nsid<S>,
        rkey: &Rkey<S>,
    ) -> Result<Arc<RecordBody>, SlingshotError>
    where
        S: BosStr,
    {
        let mut url = self
            .base
            .join(GET_RECORD_PATH)
            .expect("base url is hierarchical and GET_RECORD_PATH is a literal relative path");
        url.query_pairs_mut()
            .clear()
            .append_pair("repo", repo.as_ref())
            .append_pair("collection", collection.as_ref())
            .append_pair("rkey", rkey.as_ref());

        let resp = self
            .http
            .execute(HttpRequest {
                url,
                headers: HeaderMap::new(),
            })
            .await?;
        match resp.status {
            StatusCode::OK => {
                let bytes = read_bounded(resp).await?;
                let body = decode(&bytes)?;
                verify_addresses(&body, repo, collection, rkey)?;
                Ok(Arc::new(body))
            }
            StatusCode::NOT_FOUND => Err(SlingshotError::NotFound),
            other => Err(SlingshotError::Upstream(other)),
        }
    }
}

fn ensure_trailing_slash(mut base: Url) -> Url {
    if !base.path().ends_with('/') {
        let with_slash = format!("{}/", base.path());
        base.set_path(&with_slash);
    }
    base
}

async fn read_bounded(resp: HttpResponseHead) -> Result<Bytes, SlingshotError> {
    if resp.content_length.is_some_and(|len| len > MAX_BODY_BYTES) {
        return Err(SlingshotError::BodyTooLarge {
            limit: MAX_BODY_BYTES,
        });
    }
    let buf = resp
        .body
        .map_err(SlingshotError::Network)
        .try_fold(BytesMut::new(), |mut acc, chunk| async move {
            if (acc.len() as u64).saturating_add(chunk.len() as u64) > MAX_BODY_BYTES {
                return Err(SlingshotError::BodyTooLarge {
                    limit: MAX_BODY_BYTES,
                });
            }
            acc.extend_from_slice(&chunk);
            Ok(acc)
        })
        .await?;
    Ok(buf.freeze())
}

fn decode(bytes: &[u8]) -> Result<RecordBody, SlingshotError> {
    let raw: RawResponse<'_> = serde_json::from_slice(bytes)?;
    let value = raw.value.ok_or(SlingshotError::MissingField("value"))?;
    IpldCid::try_from(raw.cid)?;
    Ok(RecordBody {
        uri: AtUri::new_owned(raw.uri)?,
        cid: raw.cid.to_owned().into(),
        value: Bytes::copy_from_slice(value.get().as_bytes()),
    })
}

fn verify_addresses<S: BosStr>(
    body: &RecordBody,
    repo: &Did<S>,
    collection: &Nsid<S>,
    rkey: &Rkey<S>,
) -> Result<(), SlingshotError> {
    let expected = format!(
        "at://{}/{}/{}",
        repo.as_ref(),
        collection.as_ref(),
        rkey.as_ref()
    );
    if body.uri.as_ref() == expected {
        Ok(())
    } else {
        Err(SlingshotError::UriMismatch {
            expected,
            got: body.uri.as_ref().to_owned(),
        })
    }
}

#[derive(Deserialize)]
struct RawResponse<'a> {
    uri: &'a str,
    cid: &'a str,
    #[serde(default, borrow)]
    value: Option<&'a RawValue>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use jacquard_common::DefaultStr;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const VALID_CID: &str = "bafyreieqygohnz2zqyvtvktbjpvhutphobcmbsnt4q5lc36ri7vpcmoz4i";

    fn did(s: &'static str) -> Did<DefaultStr> {
        Did::new_static(s).unwrap()
    }
    fn nsid(s: &'static str) -> Nsid<DefaultStr> {
        Nsid::new_static(s).unwrap()
    }
    fn rkey(s: &str) -> Rkey<DefaultStr> {
        Rkey::new_owned(s).unwrap()
    }

    async fn server() -> MockServer {
        MockServer::start().await
    }

    fn client_for(server: &MockServer) -> SlingshotClient {
        SlingshotClient::with_default_http(Url::parse(&server.uri()).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn returns_decoded_record_on_200() {
        let server = server().await;
        let body = json!({
            "uri": "at://did:plc:abalone/sh.tangled.repo/r1",
            "cid": VALID_CID,
            "value": {
                "$type": "sh.tangled.repo",
                "knot": "oyster.cafe",
                "createdAt": "2026-05-01T00:00:00Z"
            }
        });
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.repo.getRecord"))
            .and(query_param("repo", "did:plc:abalone"))
            .and(query_param("collection", "sh.tangled.repo"))
            .and(query_param("rkey", "r1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let client = client_for(&server);
        let resp = client
            .get_record(
                &did("did:plc:abalone"),
                &nsid("sh.tangled.repo"),
                &rkey("r1"),
            )
            .await
            .unwrap();
        assert_eq!(resp.uri.as_ref(), "at://did:plc:abalone/sh.tangled.repo/r1");
        let parsed: serde_json::Value = serde_json::from_slice(&resp.value).unwrap();
        assert_eq!(parsed["knot"], "oyster.cafe");
    }

    #[tokio::test]
    async fn preserves_value_bytes_byte_for_byte() {
        let server = server().await;
        let raw = format!(
            r#"{{"uri":"at://did:plc:uni/sh.tangled.repo/r1","cid":"{VALID_CID}","value":{{"z":1,"a":2}}}}"#
        );
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.repo.getRecord"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(raw),
            )
            .mount(&server)
            .await;

        let client = client_for(&server);
        let resp = client
            .get_record(&did("did:plc:uni"), &nsid("sh.tangled.repo"), &rkey("r1"))
            .await
            .unwrap();
        assert_eq!(resp.value.as_ref(), br#"{"z":1,"a":2}"#);
    }

    #[tokio::test]
    async fn maps_404_to_not_found() {
        let server = server().await;
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.repo.getRecord"))
            .respond_with(ResponseTemplate::new(404).set_body_string("nope"))
            .mount(&server)
            .await;

        let client = client_for(&server);
        let err = client
            .get_record(
                &did("did:plc:abalone"),
                &nsid("sh.tangled.repo"),
                &rkey("r1"),
            )
            .await
            .expect_err("404 must surface");
        assert!(matches!(err, SlingshotError::NotFound));
    }

    #[tokio::test]
    async fn maps_400_to_upstream() {
        let server = server().await;
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.repo.getRecord"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad"))
            .mount(&server)
            .await;

        let client = client_for(&server);
        let err = client
            .get_record(
                &did("did:plc:abalone"),
                &nsid("sh.tangled.repo"),
                &rkey("r1"),
            )
            .await
            .expect_err("400 must surface as upstream, not not-found");
        match err {
            SlingshotError::Upstream(s) => assert_eq!(s.as_u16(), 400),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn maps_5xx_to_upstream() {
        let server = server().await;
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.repo.getRecord"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let client = client_for(&server);
        let err = client
            .get_record(
                &did("did:plc:abalone"),
                &nsid("sh.tangled.repo"),
                &rkey("r1"),
            )
            .await
            .expect_err("5xx must surface");
        match err {
            SlingshotError::Upstream(s) => assert_eq!(s.as_u16(), 503),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_oversize_body_via_content_length() {
        let server = server().await;
        let payload = vec![b'x'; (MAX_BODY_BYTES + 1) as usize];
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.repo.getRecord"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_bytes(payload),
            )
            .mount(&server)
            .await;

        let client = client_for(&server);
        let err = client
            .get_record(
                &did("did:plc:abalone"),
                &nsid("sh.tangled.repo"),
                &rkey("r1"),
            )
            .await
            .expect_err("oversize body must be rejected");
        assert!(
            matches!(err, SlingshotError::BodyTooLarge { limit } if limit == MAX_BODY_BYTES),
            "wrong variant: {err:?}"
        );
    }

    #[tokio::test]
    async fn rejects_garbage_cid() {
        let server = server().await;
        let body = json!({
            "uri": "at://did:plc:abalone/sh.tangled.repo/r1",
            "cid": "not-a-real-cid",
            "value": {"$type": "sh.tangled.repo", "knot": "oyster.cafe", "createdAt": "2026-05-01T00:00:00Z"}
        });
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.repo.getRecord"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let client = client_for(&server);
        let err = client
            .get_record(
                &did("did:plc:abalone"),
                &nsid("sh.tangled.repo"),
                &rkey("r1"),
            )
            .await
            .expect_err("garbage cid must be rejected");
        assert!(
            matches!(err, SlingshotError::InvalidCid(_)),
            "wrong variant: {err:?}"
        );
    }

    #[tokio::test]
    async fn rejects_uri_mismatch() {
        let server = server().await;
        let body = json!({
            "uri": "at://did:plc:limpet/sh.tangled.repo/elsewhere",
            "cid": VALID_CID,
            "value": {"$type": "sh.tangled.repo", "knot": "oyster.cafe", "createdAt": "2026-05-01T00:00:00Z"}
        });
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.repo.getRecord"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let client = client_for(&server);
        let err = client
            .get_record(
                &did("did:plc:abalone"),
                &nsid("sh.tangled.repo"),
                &rkey("r1"),
            )
            .await
            .expect_err("uri mismatch must be rejected");
        match err {
            SlingshotError::UriMismatch { expected, got } => {
                assert_eq!(expected, "at://did:plc:abalone/sh.tangled.repo/r1");
                assert_eq!(got, "at://did:plc:limpet/sh.tangled.repo/elsewhere");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_explicit_null_value() {
        let server = server().await;
        let body = json!({
            "uri": "at://did:plc:abalone/sh.tangled.repo/r1",
            "cid": VALID_CID,
            "value": null
        });
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.repo.getRecord"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let client = client_for(&server);
        let err = client
            .get_record(
                &did("did:plc:abalone"),
                &nsid("sh.tangled.repo"),
                &rkey("r1"),
            )
            .await
            .expect_err("explicit null value must be rejected");
        assert!(
            matches!(err, SlingshotError::MissingField("value")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn rejects_missing_value_field() {
        let server = server().await;
        let body = json!({
            "uri": "at://did:plc:abalone/sh.tangled.repo/r1",
            "cid": VALID_CID
        });
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.repo.getRecord"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let client = client_for(&server);
        let err = client
            .get_record(
                &did("did:plc:abalone"),
                &nsid("sh.tangled.repo"),
                &rkey("r1"),
            )
            .await
            .expect_err("missing value must be rejected");
        assert!(
            matches!(err, SlingshotError::MissingField("value")),
            "{err:?}"
        );
    }

    #[test]
    fn rejects_non_http_scheme() {
        let err = SlingshotClient::with_default_http(Url::parse("ftp://nel.pet").unwrap())
            .expect_err("ftp bad");
        assert!(matches!(err, SlingshotError::BadScheme(s) if s == "ftp"));
    }

    #[tokio::test]
    async fn preserves_operator_configured_base_path() {
        let server = server().await;
        let body = json!({
            "uri": "at://did:plc:abalone/sh.tangled.repo/r1",
            "cid": VALID_CID,
            "value": {
                "$type": "sh.tangled.repo",
                "knot": "oyster.cafe",
                "createdAt": "2026-05-01T00:00:00Z"
            }
        });
        Mock::given(method("GET"))
            .and(path("/api/v0/xrpc/com.atproto.repo.getRecord"))
            .and(query_param("repo", "did:plc:abalone"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let base = Url::parse(&format!("{}/api/v0", server.uri())).unwrap();
        let client = SlingshotClient::with_default_http(base).unwrap();
        client
            .get_record(
                &did("did:plc:abalone"),
                &nsid("sh.tangled.repo"),
                &rkey("r1"),
            )
            .await
            .expect("path-prefixed base must reach prefixed xrpc endpoint");
    }

    #[tokio::test]
    async fn accepts_borrowed_string_newtypes() {
        let server = server().await;
        let body = json!({
            "uri": "at://did:plc:limpet/sh.tangled.repo/r1",
            "cid": VALID_CID,
            "value": {
                "$type": "sh.tangled.repo",
                "knot": "nel.pet",
                "createdAt": "2026-05-01T00:00:00Z"
            }
        });
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.repo.getRecord"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let uri = AtUri::<DefaultStr>::new_owned("at://did:plc:limpet/sh.tangled.repo/r1").unwrap();
        let collection = uri.collection().unwrap();
        let rkey = uri.rkey().unwrap();
        let did_borrow = match uri.authority() {
            jacquard_common::types::ident::AtIdentifier::Did(d) => d,
            jacquard_common::types::ident::AtIdentifier::Handle(_) => unreachable!(),
        };

        let client = client_for(&server);
        let resp = client
            .get_record(&did_borrow, &collection, &rkey)
            .await
            .unwrap();
        assert_eq!(resp.uri.as_ref(), "at://did:plc:limpet/sh.tangled.repo/r1");
    }
}
