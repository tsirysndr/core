use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bobbin_knot_proxy::{KnotHost, KnotHostError, PrivateAddressFilter, PrivateHostReason};
use bobbin_runtime::{HttpRequest, HttpResponseHead, HttpTransport, NetworkError, ReqwestHttp};
use bytes::{Bytes, BytesMut};
use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use http::{HeaderMap, StatusCode};
use jacquard_common::DefaultStr;
use jacquard_common::types::did::Did;
use jacquard_common::types::nsid::Nsid;
use serde::Deserialize;
use thiserror::Error;
use url::Url;

const USER_AGENT: &str = concat!("bobbin/", env!("CARGO_PKG_VERSION"));
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BODY_BYTES: u64 = 4 * 1024 * 1024;
const LIST_PAGE_LIMIT: i64 = 1000;
const MAX_LIST_PAGES: usize = 256;

const VERSION_NSID: &str = "sh.tangled.knot.version";
const LIST_MEMBERS_NSID: &str = "sh.tangled.knot.listMembers";
const LIST_COLLABORATORS_NSID: &str = "sh.tangled.repo.listCollaborators";

#[derive(Clone)]
pub struct KnotClient {
    http: Arc<dyn HttpTransport>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AclEntry {
    pub subject: Did<DefaultStr>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Completeness {
    Complete,
    Truncated,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AclListing {
    pub entries: Vec<AclEntry>,
    pub completeness: Completeness,
}

#[derive(Debug, Error)]
pub enum KnotClientError {
    #[error("knot host: {0}")]
    Host(#[from] KnotHostError),
    #[error("blocked: knot {host} resolves to {reason} address space")]
    PrivateHost {
        host: String,
        reason: PrivateHostReason,
    },
    #[error("http client build: {0}")]
    Build(String),
    #[error("network: {0}")]
    Network(#[from] NetworkError),
    #[error("xrpc not found")]
    NotFound,
    #[error("upstream returned status {0}")]
    Upstream(StatusCode),
    #[error("response body exceeded {limit} bytes")]
    BodyTooLarge { limit: u64 },
    #[error("decode response: {0}")]
    Decode(#[from] serde_json::Error),
}

pub fn knot_endpoint(
    host: &str,
    dev: bool,
    allow_private: bool,
) -> Result<KnotHost, KnotClientError> {
    let raw = if dev {
        format!("http://{host}")
    } else {
        host.to_owned()
    };
    let knot = KnotHost::parse(&raw)?;
    if !allow_private && let Some(reason) = knot.private_literal_reason() {
        return Err(KnotClientError::PrivateHost {
            host: host.to_owned(),
            reason,
        });
    }
    Ok(knot)
}

fn default_http_client(allow_private: bool) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(Arc::new(PrivateAddressFilter::new(allow_private)))
        .build()
}

fn nsid(s: &'static str) -> Nsid<DefaultStr> {
    Nsid::new_static(s).expect("static nsid literal must validate")
}

pub(crate) fn authority(host: &KnotHost) -> String {
    let url = host.url();
    match (url.host_str(), url.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_owned(),
        (None, _) => String::new(),
    }
}

impl KnotClient {
    pub fn new(http: Arc<dyn HttpTransport>) -> Self {
        Self { http }
    }

    pub fn with_default_http(allow_private: bool) -> Result<Self, KnotClientError> {
        let client = default_http_client(allow_private)
            .map_err(|e| KnotClientError::Build(e.to_string()))?;
        Ok(Self::new(ReqwestHttp::shared(client)))
    }

    pub async fn capabilities(&self, host: &KnotHost) -> Result<Vec<String>, KnotClientError> {
        let mut url = host.xrpc_url(&nsid(VERSION_NSID));
        url.set_query(None);
        let bytes = self.get_json(url).await?;
        let resp: VersionWire = serde_json::from_slice(&bytes)?;
        Ok(resp.capabilities.unwrap_or_default())
    }

    pub async fn list_members(&self, host: &KnotHost) -> Result<AclListing, KnotClientError> {
        let subject = authority(host);
        self.drain(host, LIST_MEMBERS_NSID, subject, None, 0, Vec::new())
            .await
    }

    pub async fn list_collaborators(
        &self,
        host: &KnotHost,
        repo: &Did<DefaultStr>,
    ) -> Result<AclListing, KnotClientError> {
        self.drain(
            host,
            LIST_COLLABORATORS_NSID,
            repo.as_ref().to_owned(),
            None,
            0,
            Vec::new(),
        )
        .await
    }

    fn drain<'a>(
        &'a self,
        host: &'a KnotHost,
        endpoint: &'static str,
        subject: String,
        cursor: Option<String>,
        page: usize,
        mut acc: Vec<AclEntry>,
    ) -> Pin<Box<dyn Future<Output = Result<AclListing, KnotClientError>> + Send + 'a>> {
        Box::pin(async move {
            if page >= MAX_LIST_PAGES {
                tracing::warn!(
                    host = %authority(host),
                    endpoint,
                    pages = page,
                    "knot list truncated at page cap"
                );
                return Ok(AclListing {
                    entries: acc,
                    completeness: Completeness::Truncated,
                });
            }
            let resp = self
                .fetch_page(host, endpoint, &subject, cursor.as_deref())
                .await?;
            acc.extend(resp.items);
            match resp.cursor.filter(|c| !c.is_empty()) {
                Some(next) => {
                    self.drain(host, endpoint, subject, Some(next), page + 1, acc)
                        .await
                }
                None => Ok(AclListing {
                    entries: acc,
                    completeness: Completeness::Complete,
                }),
            }
        })
    }

    async fn fetch_page(
        &self,
        host: &KnotHost,
        endpoint: &'static str,
        subject: &str,
        cursor: Option<&str>,
    ) -> Result<ListWire, KnotClientError> {
        let mut url = host.xrpc_url(&nsid(endpoint));
        {
            let mut q = url.query_pairs_mut();
            q.clear();
            q.append_pair("subject", subject);
            q.append_pair("limit", &LIST_PAGE_LIMIT.to_string());
            if let Some(c) = cursor {
                q.append_pair("cursor", c);
            }
        }
        let bytes = self.get_json(url).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn get_json(&self, url: Url) -> Result<Bytes, KnotClientError> {
        let resp = self
            .http
            .execute(HttpRequest {
                url,
                headers: HeaderMap::new(),
            })
            .await?;
        match resp.status {
            StatusCode::OK => read_bounded(resp).await,
            StatusCode::NOT_FOUND => Err(KnotClientError::NotFound),
            other => Err(KnotClientError::Upstream(other)),
        }
    }
}

#[derive(Deserialize)]
struct VersionWire {
    #[serde(default)]
    capabilities: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct ListWire {
    #[serde(default)]
    items: Vec<AclEntry>,
    #[serde(default)]
    cursor: Option<String>,
}

async fn read_bounded(resp: HttpResponseHead) -> Result<Bytes, KnotClientError> {
    if resp.content_length.is_some_and(|len| len > MAX_BODY_BYTES) {
        return Err(KnotClientError::BodyTooLarge {
            limit: MAX_BODY_BYTES,
        });
    }
    let buf = resp
        .body
        .map_err(KnotClientError::Network)
        .try_fold(BytesMut::new(), |mut acc, chunk| async move {
            if (acc.len() as u64).saturating_add(chunk.len() as u64) > MAX_BODY_BYTES {
                return Err(KnotClientError::BodyTooLarge {
                    limit: MAX_BODY_BYTES,
                });
            }
            acc.extend_from_slice(&chunk);
            Ok(acc)
        })
        .await?;
    Ok(buf.freeze())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn did(s: &str) -> Did<DefaultStr> {
        Did::new_owned(s).unwrap()
    }

    fn client() -> KnotClient {
        KnotClient::new(ReqwestHttp::shared(default_http_client(true).unwrap()))
    }

    fn endpoint(server: &MockServer) -> KnotHost {
        KnotHost::parse(&server.uri()).unwrap()
    }

    #[tokio::test]
    async fn capabilities_returns_declared_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.knot.version"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "version": "1.0.0 (deadbeef)",
                "capabilities": ["knot-acl"]
            })))
            .mount(&server)
            .await;

        let caps = client().capabilities(&endpoint(&server)).await.unwrap();
        assert_eq!(caps, vec!["knot-acl".to_owned()]);
    }

    #[tokio::test]
    async fn capabilities_empty_when_field_absent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.knot.version"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "version": "1.0.0 (cafe)" })),
            )
            .mount(&server)
            .await;

        let caps = client().capabilities(&endpoint(&server)).await.unwrap();
        assert!(caps.is_empty());
    }

    #[tokio::test]
    async fn list_members_drains_single_page() {
        let server = MockServer::start().await;
        let host = endpoint(&server);
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.knot.listMembers"))
            .and(query_param("subject", authority(&host).as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [
                    {"subject": "did:plc:boltless", "addedBy": "did:plc:akshay", "createdAt": "2026-06-01T00:00:00Z"},
                    {"subject": "did:plc:akshay", "addedBy": "did:plc:akshay", "createdAt": "2026-06-02T12:00:00Z"}
                ]
            })))
            .mount(&server)
            .await;

        let listing = client().list_members(&host).await.unwrap();
        assert_eq!(listing.completeness, Completeness::Complete);
        assert_eq!(
            listing.entries,
            vec![
                AclEntry {
                    subject: did("did:plc:boltless"),
                    created_at: "2026-06-01T00:00:00Z".parse().unwrap(),
                },
                AclEntry {
                    subject: did("did:plc:akshay"),
                    created_at: "2026-06-02T12:00:00Z".parse().unwrap(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn list_members_drains_multiple_pages() {
        let server = MockServer::start().await;
        let host = endpoint(&server);
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.knot.listMembers"))
            .and(query_param("cursor", "p2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [{"subject": "did:plc:akshay", "addedBy": "did:plc:akshay", "createdAt": "2026-06-02T00:00:00Z"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.knot.listMembers"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [{"subject": "did:plc:boltless", "addedBy": "did:plc:akshay", "createdAt": "2026-06-01T00:00:00Z"}],
                "cursor": "p2"
            })))
            .mount(&server)
            .await;

        let listing = client().list_members(&host).await.unwrap();
        assert_eq!(listing.completeness, Completeness::Complete);
        let subjects: Vec<_> = listing.entries.into_iter().map(|m| m.subject).collect();
        assert_eq!(
            subjects,
            vec![did("did:plc:boltless"), did("did:plc:akshay")]
        );
    }

    #[tokio::test]
    async fn list_collaborators_uses_repo_did_subject() {
        let server = MockServer::start().await;
        let host = endpoint(&server);
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.repo.listCollaborators"))
            .and(query_param("subject", "did:plc:scallop"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [{"subject": "did:plc:olaren", "addedBy": "did:plc:boltless", "createdAt": "2026-06-03T00:00:00Z"}]
            })))
            .mount(&server)
            .await;

        let listing = client()
            .list_collaborators(&host, &did("did:plc:scallop"))
            .await
            .unwrap();
        assert_eq!(listing.completeness, Completeness::Complete);
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].subject, did("did:plc:olaren"));
    }

    #[tokio::test]
    async fn drain_reports_truncation_at_page_cap() {
        let server = MockServer::start().await;
        let host = endpoint(&server);
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.knot.listMembers"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [{"subject": "did:plc:boltless", "addedBy": "did:plc:akshay", "createdAt": "2026-06-01T00:00:00Z"}],
                "cursor": "more"
            })))
            .mount(&server)
            .await;

        let listing = client().list_members(&host).await.unwrap();
        assert_eq!(
            listing.completeness,
            Completeness::Truncated,
            "a never-terminating cursor must surface as a truncated listing"
        );
        assert_eq!(listing.entries.len(), MAX_LIST_PAGES);
    }

    #[tokio::test]
    async fn maps_404_to_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let err = client()
            .capabilities(&endpoint(&server))
            .await
            .expect_err("404 must surface");
        assert!(matches!(err, KnotClientError::NotFound));
    }

    #[tokio::test]
    async fn maps_5xx_to_upstream() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let err = client()
            .capabilities(&endpoint(&server))
            .await
            .expect_err("5xx must surface");
        match err {
            KnotClientError::Upstream(s) => assert_eq!(s.as_u16(), 503),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn does_not_follow_redirects() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.knot.version"))
            .respond_with(ResponseTemplate::new(301).insert_header(
                "location",
                "https://kt.tngl.oyster.cafe/xrpc/sh.tangled.knot.version",
            ))
            .mount(&server)
            .await;

        let err = client()
            .capabilities(&endpoint(&server))
            .await
            .expect_err("a redirect must surface as an error, not be followed to another knot");
        match err {
            KnotClientError::Upstream(s) => assert_eq!(s.as_u16(), 301),
            other => panic!("expected Upstream(301), got {other:?}"),
        }
    }

    #[test]
    fn knot_endpoint_rejects_private_host() {
        let err =
            knot_endpoint("127.0.0.1:9", true, false).expect_err("private host must be refused");
        assert!(matches!(err, KnotClientError::PrivateHost { .. }));
    }

    #[test]
    fn knot_endpoint_allows_private_when_permitted() {
        let knot = knot_endpoint("127.0.0.1:9", true, true).expect("private host allowed");
        assert_eq!(knot.url().scheme(), "http");
    }
}
