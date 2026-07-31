use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use bobbin_edge_index::{CoverageWatch, EdgeStore, StateIndex};
use bobbin_knot_proxy::{FailureThreshold, KnotHttpConfig, KnotProxy, KnotProxyConfig};
use bobbin_record_lru::{CacheCapacity, LruRecordStore};
use bobbin_resolver::RepoIdResolver;
use bobbin_runtime::{RuntimeHasher, SystemClock};
use bobbin_search::{DEFAULT_WRITER_HEAP_BYTES, SearchIndex, SearchReader};
use bobbin_slingshot_client::SlingshotClient;
use bobbin_xrpc::{AppState, router};
use http::{HeaderName, HeaderValue, Request, StatusCode};
use jacquard_common::DefaultStr;
use jacquard_common::types::did::Did;
use jacquard_common::types::recordkey::Rkey;
use serde_json::{Value, json};
use tower::ServiceExt;
use trusted_proxies::TrustedProxies;
use url::Url;
use url::form_urlencoded::byte_serialize;
use wiremock::matchers::{header_exists, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CID: &str = "bafyreieqygohnz2zqyvtvktbjpvhutphobcmbsnt4q5lc36ri7vpcmoz4i";

const SOCKET: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4321);

fn did(s: &str) -> Did<DefaultStr> {
    Did::new_owned(s).unwrap()
}

fn rkey(s: &str) -> Rkey<DefaultStr> {
    Rkey::new_owned(s).unwrap()
}

fn hdr(name: &'static str, value: &'static str) -> (HeaderName, HeaderValue) {
    (
        HeaderName::from_static(name),
        HeaderValue::from_static(value),
    )
}

fn test_config() -> KnotProxyConfig {
    KnotProxyConfig {
        failure_threshold: FailureThreshold::new(2).unwrap(),
        cooldown: Duration::from_millis(80),
        allow_private_hosts: true,
        require_https: false,
    }
}

fn test_http_config() -> KnotHttpConfig {
    KnotHttpConfig {
        connect_timeout: Duration::from_millis(500),
        read_timeout: Duration::from_secs(2),
    }
}

struct Harness {
    slingshot: MockServer,
    knot: MockServer,
    state: AppState,
}

impl Harness {
    async fn new() -> Self {
        Self::with_config(test_config()).await
    }

    async fn behind_proxy() -> Self {
        let harness = Self::with_config(test_config()).await;
        Self {
            state: harness
                .state
                .clone()
                .with_proxies(TrustedProxies::parse(["127.0.0.1"]).unwrap()),
            ..harness
        }
    }

    async fn with_config(config: KnotProxyConfig) -> Self {
        let slingshot_server = MockServer::start().await;
        let knot_server = MockServer::start().await;
        let state = AppState::new(
            Arc::new(LruRecordStore::new(CacheCapacity::from_bytes(64 * 1024))),
            SlingshotClient::with_default_http(Url::parse(&slingshot_server.uri()).unwrap())
                .unwrap(),
            Arc::new(EdgeStore::new(RuntimeHasher::default())),
            Arc::new(StateIndex::new(RuntimeHasher::default())),
            Arc::new(StateIndex::new(RuntimeHasher::default())),
            Arc::new(CoverageWatch::new()),
            Arc::new(
                KnotProxy::new(
                    config,
                    test_http_config(),
                    Arc::new(SystemClock::new()),
                    RuntimeHasher::default(),
                )
                .unwrap(),
            ),
            Arc::new(
                SearchIndex::new(DEFAULT_WRITER_HEAP_BYTES, Arc::new(SystemClock::new())).unwrap(),
            ) as Arc<dyn SearchReader>,
            Arc::new(RepoIdResolver::detached(RuntimeHasher::default())),
        );
        Self {
            slingshot: slingshot_server,
            knot: knot_server,
            state,
        }
    }

    async fn mount_repo_record(&self, did: &Did<DefaultStr>, rkey: &Rkey<DefaultStr>, name: &str) {
        self.mount_repo_record_inner(did, rkey, Some(name)).await;
    }

    async fn mount_repo_record_rkey_as_name(&self, did: &Did<DefaultStr>, rkey: &Rkey<DefaultStr>) {
        self.mount_repo_record_inner(did, rkey, None).await;
    }

    async fn mount_repo_record_inner(
        &self,
        did: &Did<DefaultStr>,
        rkey: &Rkey<DefaultStr>,
        name: Option<&str>,
    ) {
        let knot_value = self.knot.uri();
        let mut record = json!({
            "$type": "sh.tangled.repo",
            "createdAt": "2026-05-01T00:00:00Z",
            "knot": knot_value,
        });
        if let Some(n) = name {
            record["name"] = json!(n);
        }
        let uri = format!("at://{}/sh.tangled.repo/{}", did.as_ref(), rkey.as_ref());
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.repo.getRecord"))
            .and(query_param("repo", did.as_ref()))
            .and(query_param("collection", "sh.tangled.repo"))
            .and(query_param("rkey", rkey.as_ref()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "uri": uri,
                "cid": CID,
                "value": record,
            })))
            .mount(&self.slingshot)
            .await;
    }

    async fn call(&self, path_and_query: &str) -> http::Response<Body> {
        self.call_with_headers(path_and_query, &[]).await
    }

    async fn call_with_headers(
        &self,
        path_and_query: &str,
        client_headers: &[(HeaderName, HeaderValue)],
    ) -> http::Response<Body> {
        self.call_from(path_and_query, client_headers, Some(SOCKET))
            .await
    }

    async fn blob_client_address(&self, tid: &str, socket: Option<SocketAddr>) -> Option<String> {
        self.mount_repo_record(&did("did:plc:limpet"), &rkey(tid), "kelp")
            .await;
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.repo.blob"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(r#"{"path":"x"}"#, "application/json"),
            )
            .mount(&self.knot)
            .await;
        let target = format!(
            "/xrpc/sh.tangled.repo.blob?repo={}&path=x",
            enc(&format!("at://did:plc:limpet/sh.tangled.repo/{tid}")),
        );
        let resp = self
            .call_from(&target, &[hdr("x-forwarded-for", "203.0.113.42")], socket)
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        self.knot
            .received_requests()
            .await
            .unwrap()
            .iter()
            .find(|r| r.url.path() == "/xrpc/sh.tangled.repo.blob")
            .expect("knot received the proxied call")
            .headers
            .get("x-forwarded-for")
            .map(|value| value.to_str().unwrap().to_owned())
    }

    async fn call_from(
        &self,
        path_and_query: &str,
        client_headers: &[(HeaderName, HeaderValue)],
        socket: Option<SocketAddr>,
    ) -> http::Response<Body> {
        let connected = socket
            .into_iter()
            .fold(Request::builder().uri(path_and_query), |b, socket| {
                b.extension(ConnectInfo(socket))
            });
        let builder = client_headers
            .iter()
            .fold(connected, |b, (name, value)| b.header(name, value));
        router(self.state.clone())
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .expect("router infallible")
    }
}

fn enc(s: &str) -> String {
    byte_serialize(s.as_bytes()).collect()
}

async fn body_string(resp: http::Response<Body>) -> String {
    let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    String::from_utf8(body.to_vec()).expect("response body is utf-8")
}

async fn body_value(resp: http::Response<Body>) -> Value {
    let s = body_string(resp).await;
    serde_json::from_str(&s).unwrap_or_else(|e| panic!("body not json: {e}: {s}"))
}

#[tokio::test]
async fn proxies_repo_blob_with_did_slash_name_repo_param() {
    let h = Harness::new().await;
    let tid = "3jzfcijpj2z2a";
    h.mount_repo_record(&did("did:plc:abalone"), &rkey(tid), "barnacle")
        .await;
    Mock::given(method("GET"))
        .and(path("/xrpc/sh.tangled.repo.blob"))
        .and(query_param("repo", "did:plc:abalone/barnacle"))
        .and(query_param("ref", "main"))
        .and(query_param("path", "README.md"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(r#"{"path":"README.md","content":"hi"}"#, "application/json"),
        )
        .mount(&h.knot)
        .await;

    let target = format!(
        "/xrpc/sh.tangled.repo.blob?repo={}&ref=main&path=README.md",
        enc(&format!("at://did:plc:abalone/sh.tangled.repo/{tid}")),
    );
    let resp = h.call(&target).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/json",
    );
    let v = body_value(resp).await;
    assert_eq!(v["path"], "README.md");
    assert_eq!(v["content"], "hi");
}

#[tokio::test]
async fn modern_rkey_as_name_uses_rkey_even_when_name_field_set() {
    let h = Harness::new().await;
    h.mount_repo_record(&did("did:plc:abalone"), &rkey("core"), "Tangled Core")
        .await;
    Mock::given(method("GET"))
        .and(path("/xrpc/sh.tangled.repo.getDefaultBranch"))
        .and(query_param("repo", "did:plc:abalone/core"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "hash": "abc",
            "name": "main",
            "when": "2026-05-01T00:00:00Z",
        })))
        .mount(&h.knot)
        .await;

    let target = format!(
        "/xrpc/sh.tangled.repo.getDefaultBranch?repo={}",
        enc("at://did:plc:abalone/sh.tangled.repo/core"),
    );
    let resp = h.call(&target).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_value(resp).await;
    assert_eq!(v["name"], "main");
}

#[tokio::test]
async fn modern_rkey_as_name_works_when_name_field_null() {
    let h = Harness::new().await;
    h.mount_repo_record_rkey_as_name(&did("did:plc:abalone"), &rkey("core"))
        .await;
    Mock::given(method("GET"))
        .and(path("/xrpc/sh.tangled.repo.getDefaultBranch"))
        .and(query_param("repo", "did:plc:abalone/core"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "hash": "abc",
            "name": "main",
            "when": "2026-05-01T00:00:00Z",
        })))
        .mount(&h.knot)
        .await;

    let target = format!(
        "/xrpc/sh.tangled.repo.getDefaultBranch?repo={}",
        enc("at://did:plc:abalone/sh.tangled.repo/core"),
    );
    let resp = h.call(&target).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn legacy_tid_rkey_falls_back_to_name_field() {
    let h = Harness::new().await;
    let tid_rkey = "3jzfcijpj2z2a";
    h.mount_repo_record(&did("did:plc:abalone"), &rkey(tid_rkey), "dotfiles")
        .await;
    Mock::given(method("GET"))
        .and(path("/xrpc/sh.tangled.repo.getDefaultBranch"))
        .and(query_param("repo", "did:plc:abalone/dotfiles"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "hash": "abc",
            "name": "main",
            "when": "2026-05-01T00:00:00Z",
        })))
        .mount(&h.knot)
        .await;

    let target = format!(
        "/xrpc/sh.tangled.repo.getDefaultBranch?repo={}",
        enc(&format!("at://did:plc:abalone/sh.tangled.repo/{tid_rkey}")),
    );
    let resp = h.call(&target).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn tid_rkey_without_name_falls_back_to_tid() {
    let h = Harness::new().await;
    let tid_rkey = "3jzfcijpj2z2a";
    h.mount_repo_record_rkey_as_name(&did("did:plc:abalone"), &rkey(tid_rkey))
        .await;
    Mock::given(method("GET"))
        .and(path("/xrpc/sh.tangled.repo.getDefaultBranch"))
        .and(query_param("repo", format!("did:plc:abalone/{tid_rkey}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "hash": "abc",
            "name": "main",
            "when": "2026-05-01T00:00:00Z",
        })))
        .mount(&h.knot)
        .await;
    let target = format!(
        "/xrpc/sh.tangled.repo.getDefaultBranch?repo={}",
        enc(&format!("at://did:plc:abalone/sh.tangled.repo/{tid_rkey}")),
    );
    let resp = h.call(&target).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn streams_binary_archive_through_proxy() {
    let h = Harness::new().await;
    let tid = "3jzfcijpj2z2b";
    h.mount_repo_record(&did("did:plc:limpet"), &rkey(tid), "kelp")
        .await;
    let payload: Vec<u8> = (0u8..=255).collect();
    Mock::given(method("GET"))
        .and(path("/xrpc/sh.tangled.repo.archive"))
        .and(query_param("repo", "did:plc:limpet/kelp"))
        .and(query_param("ref", "v1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/gzip")
                .set_body_bytes(payload.clone()),
        )
        .mount(&h.knot)
        .await;

    let target = format!(
        "/xrpc/sh.tangled.repo.archive?repo={}&ref=v1",
        enc(&format!("at://did:plc:limpet/sh.tangled.repo/{tid}")),
    );
    let resp = h.call(&target).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/gzip",
    );
    let body = to_bytes(resp.into_body(), 4 * 1024).await.unwrap();
    assert_eq!(body.as_ref(), payload.as_slice());
}

#[tokio::test]
async fn missing_repo_param_returns_400() {
    let h = Harness::new().await;
    let resp = h.call("/xrpc/sh.tangled.repo.blob?ref=main").await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_value(resp).await;
    assert_eq!(v["error"], "InvalidRequest");
}

#[tokio::test]
async fn unknown_repo_propagates_404_from_slingshot() {
    let h = Harness::new().await;
    Mock::given(method("GET"))
        .and(path("/xrpc/com.atproto.repo.getRecord"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&h.slingshot)
        .await;
    let target = format!(
        "/xrpc/sh.tangled.repo.blob?repo={}&ref=main",
        enc("at://did:plc:abalone/sh.tangled.repo/missing"),
    );
    let resp = h.call(&target).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let v = body_value(resp).await;
    assert_eq!(v["error"], "RecordNotFound");
}

#[tokio::test]
async fn knot_5xx_routes_to_upstream_failed() {
    let h = Harness::new().await;
    h.mount_repo_record(&did("did:plc:abalone"), &rkey("r1"), "barnacle")
        .await;
    Mock::given(method("GET"))
        .and(path("/xrpc/sh.tangled.repo.blob"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.knot)
        .await;
    let target = format!(
        "/xrpc/sh.tangled.repo.blob?repo={}&ref=main&path=x",
        enc("at://did:plc:abalone/sh.tangled.repo/r1"),
    );
    let resp = h.call(&target).await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let v = body_value(resp).await;
    assert_eq!(v["error"], "UpstreamFailed");
}

#[tokio::test]
async fn knot_4xx_passes_through_unchanged() {
    let h = Harness::new().await;
    h.mount_repo_record(&did("did:plc:abalone"), &rkey("r1"), "barnacle")
        .await;
    Mock::given(method("GET"))
        .and(path("/xrpc/sh.tangled.repo.blob"))
        .respond_with(ResponseTemplate::new(404).set_body_raw(
            r#"{"error":"FileNotFound","message":"nope"}"#,
            "application/json",
        ))
        .mount(&h.knot)
        .await;
    let target = format!(
        "/xrpc/sh.tangled.repo.blob?repo={}&ref=main&path=missing",
        enc("at://did:plc:abalone/sh.tangled.repo/r1"),
    );
    let resp = h.call(&target).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let v = body_value(resp).await;
    assert_eq!(v["error"], "FileNotFound");
}

#[tokio::test]
async fn breaker_opens_after_threshold_then_short_circuits() {
    let h = Harness::new().await;
    h.mount_repo_record(&did("did:plc:abalone"), &rkey("r1"), "barnacle")
        .await;
    Mock::given(method("GET"))
        .and(path("/xrpc/sh.tangled.repo.blob"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.knot)
        .await;
    let target = format!(
        "/xrpc/sh.tangled.repo.blob?repo={}&ref=main&path=x",
        enc("at://did:plc:abalone/sh.tangled.repo/r1"),
    );
    let r1 = h.call(&target).await;
    assert_eq!(r1.status(), StatusCode::BAD_GATEWAY);
    let _ = body_string(r1).await;
    let r2 = h.call(&target).await;
    assert_eq!(r2.status(), StatusCode::BAD_GATEWAY);
    let _ = body_string(r2).await;
    let r3 = h.call(&target).await;
    assert_eq!(r3.status(), StatusCode::BAD_GATEWAY);
    let v = body_value(r3).await;
    assert!(
        v["message"]
            .as_str()
            .unwrap_or_default()
            .contains("circuit breaker open"),
        "third call must be short-circuited by breaker, got {v}",
    );
}

#[tokio::test]
async fn proxy_owner_uses_knot_query_param() {
    let h = Harness::new().await;
    Mock::given(method("GET"))
        .and(path("/xrpc/sh.tangled.owner"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(r#"{"owner":"did:plc:nautilus"}"#, "application/json"),
        )
        .mount(&h.knot)
        .await;
    let target = format!("/xrpc/sh.tangled.owner?knot={}", enc(&h.knot.uri()));
    let resp = h.call(&target).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_value(resp).await;
    assert_eq!(v["owner"], "did:plc:nautilus");
}

#[tokio::test]
async fn proxy_knot_version_uses_knot_query_param() {
    let h = Harness::new().await;
    Mock::given(method("GET"))
        .and(path("/xrpc/sh.tangled.knot.version"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(r#"{"version":"0.42"}"#, "application/json"),
        )
        .mount(&h.knot)
        .await;
    let target = format!("/xrpc/sh.tangled.knot.version?knot={}", enc(&h.knot.uri()));
    let resp = h.call(&target).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_value(resp).await;
    assert_eq!(v["version"], "0.42");
}

#[tokio::test]
async fn proxy_knot_list_keys_forwards_pagination_params() {
    let h = Harness::new().await;
    Mock::given(method("GET"))
        .and(path("/xrpc/sh.tangled.knot.listKeys"))
        .and(query_param("limit", "5"))
        .and(query_param("cursor", "abc"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(r#"{"keys":[]}"#, "application/json"))
        .mount(&h.knot)
        .await;
    let target = format!(
        "/xrpc/sh.tangled.knot.listKeys?knot={}&limit=5&cursor=abc",
        enc(&h.knot.uri()),
    );
    let resp = h.call(&target).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn missing_knot_param_on_knot_route_returns_400() {
    let h = Harness::new().await;
    let resp = h.call("/xrpc/sh.tangled.knot.version").await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_value(resp).await;
    assert_eq!(v["error"], "InvalidRequest");
}

#[tokio::test]
async fn second_proxy_call_skips_slingshot_via_lru() {
    let h = Harness::new().await;
    let tid = "3jzfcijpj2z2c";
    h.mount_repo_record(&did("did:plc:abalone"), &rkey(tid), "barnacle")
        .await;
    Mock::given(method("GET"))
        .and(path("/xrpc/sh.tangled.repo.tree"))
        .and(query_param("repo", "did:plc:abalone/barnacle"))
        .and(query_param("ref", "main"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(r#"{"ref":"main","files":[]}"#, "application/json"),
        )
        .mount(&h.knot)
        .await;
    let target = format!(
        "/xrpc/sh.tangled.repo.tree?repo={}&ref=main",
        enc(&format!("at://did:plc:abalone/sh.tangled.repo/{tid}")),
    );
    let r1 = h.call(&target).await;
    assert_eq!(r1.status(), StatusCode::OK);
    let _ = body_string(r1).await;
    let r2 = h.call(&target).await;
    assert_eq!(r2.status(), StatusCode::OK);
    let _ = body_string(r2).await;
    let received = h.slingshot.received_requests().await.unwrap();
    let getrecord = received
        .iter()
        .filter(|r| r.url.path() == "/xrpc/com.atproto.repo.getRecord")
        .count();
    assert_eq!(
        getrecord, 1,
        "slingshot must be hit exactly once because the LRU serves the second proxy call",
    );
}

#[tokio::test]
async fn does_not_inject_auth_or_atproto_proxy_headers() {
    let h = Harness::new().await;
    h.mount_repo_record(&did("did:plc:abalone"), &rkey("r1"), "barnacle")
        .await;
    Mock::given(method("GET"))
        .and(path("/xrpc/sh.tangled.repo.blob"))
        .and(header_exists("user-agent"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(r#"{"path":"x"}"#, "application/json"),
        )
        .mount(&h.knot)
        .await;
    let target = format!(
        "/xrpc/sh.tangled.repo.blob?repo={}&ref=main&path=x",
        enc("at://did:plc:abalone/sh.tangled.repo/r1"),
    );
    let resp = h.call(&target).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let received = h.knot.received_requests().await.unwrap();
    let knot_call = received
        .iter()
        .find(|r| r.url.path() == "/xrpc/sh.tangled.repo.blob")
        .expect("knot received the proxied call");
    assert!(
        knot_call.headers.get("authorization").is_none(),
        "bobbin must not inject auth, anonymous read by design",
    );
    assert!(
        knot_call.headers.get("atproto-proxy").is_none(),
        "bobbin is not an atproto-proxy chain",
    );
    assert!(
        knot_call.headers.get("atproto-accept-labelers").is_none(),
        "bobbin does not negotiate labelers with knots",
    );
}

#[tokio::test]
async fn forwards_range_conditional_and_client_address_headers() {
    let h = Harness::behind_proxy().await;
    let tid = "3jzfcijpj2z2d";
    h.mount_repo_record(&did("did:plc:limpet"), &rkey(tid), "kelp")
        .await;
    Mock::given(method("GET"))
        .and(path("/xrpc/sh.tangled.repo.archive"))
        .and(query_param("repo", "did:plc:limpet/kelp"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-type", "application/octet-stream")
                .insert_header("content-range", "bytes 0-99/2048")
                .insert_header("accept-ranges", "bytes")
                .insert_header("etag", "\"v1\"")
                .set_body_bytes(vec![0u8; 100]),
        )
        .mount(&h.knot)
        .await;

    let target = format!(
        "/xrpc/sh.tangled.repo.archive?repo={}&ref=v1",
        enc(&format!("at://did:plc:limpet/sh.tangled.repo/{tid}")),
    );
    let resp = h
        .call_with_headers(
            &target,
            &[
                hdr("range", "bytes=0-99"),
                hdr("if-none-match", "\"old\""),
                hdr("if-modified-since", "Wed, 01 May 2026 00:00:00 GMT"),
                hdr("x-forwarded-for", "203.0.113.42"),
            ],
        )
        .await;
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        resp.headers().get("content-range").unwrap(),
        "bytes 0-99/2048"
    );
    assert_eq!(resp.headers().get("accept-ranges").unwrap(), "bytes");
    assert_eq!(resp.headers().get("etag").unwrap(), "\"v1\"");

    let received = h.knot.received_requests().await.unwrap();
    let knot_call = received
        .iter()
        .find(|r| r.url.path() == "/xrpc/sh.tangled.repo.archive")
        .expect("knot received the proxied call");
    assert_eq!(knot_call.headers.get("range").unwrap(), "bytes=0-99");
    assert_eq!(knot_call.headers.get("if-none-match").unwrap(), "\"old\"");
    assert_eq!(
        knot_call.headers.get("if-modified-since").unwrap(),
        "Wed, 01 May 2026 00:00:00 GMT",
    );
    assert_eq!(
        knot_call.headers.get("x-forwarded-for").unwrap(),
        "203.0.113.42",
    );
}

#[tokio::test]
async fn bobbin_forwards_only_a_client_address_it_can_vouch_for() {
    assert_eq!(
        Harness::new()
            .await
            .blob_client_address("3jzfcijpj2z2e", Some(SOCKET))
            .await,
        Some(SOCKET.ip().to_string()),
        "a client that writes this header itself must reach the knot under the address it connected from, since bobbin hasn't been told to trust any proxy"
    );
    assert_eq!(
        Harness::behind_proxy()
            .await
            .blob_client_address("3jzfcijpj2z2f", None)
            .await,
        None,
        "bobbin won't forward the header a client wrote or an address it made up, because a listener served without connect info doesn't leave it anything to vouch for"
    );
}

#[tokio::test]
async fn drops_disallowed_client_headers() {
    let h = Harness::new().await;
    h.mount_repo_record(&did("did:plc:abalone"), &rkey("r1"), "barnacle")
        .await;
    Mock::given(method("GET"))
        .and(path("/xrpc/sh.tangled.repo.blob"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(r#"{"path":"x"}"#, "application/json"),
        )
        .mount(&h.knot)
        .await;
    let target = format!(
        "/xrpc/sh.tangled.repo.blob?repo={}&path=x",
        enc("at://did:plc:abalone/sh.tangled.repo/r1"),
    );
    let resp = h
        .call_with_headers(
            &target,
            &[
                hdr("authorization", "Bearer secret"),
                hdr("cookie", "sid=evil"),
                hdr("x-custom", "should-not-pass"),
            ],
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let received = h.knot.received_requests().await.unwrap();
    let knot_call = received
        .iter()
        .find(|r| r.url.path() == "/xrpc/sh.tangled.repo.blob")
        .expect("knot received the proxied call");
    assert!(knot_call.headers.get("authorization").is_none());
    assert!(knot_call.headers.get("cookie").is_none());
    assert!(knot_call.headers.get("x-custom").is_none());
}

#[tokio::test]
async fn rejects_client_supplied_loopback_under_strict_config() {
    let strict = KnotProxyConfig {
        allow_private_hosts: false,
        ..test_config()
    };
    let h = Harness::with_config(strict).await;
    let resp = h
        .call(&format!(
            "/xrpc/sh.tangled.knot.version?knot={}",
            enc("http://127.0.0.1:9"),
        ))
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_value(resp).await;
    assert_eq!(v["error"], "InvalidRequest");
    let msg = v["message"].as_str().unwrap_or_default().to_owned();
    assert!(
        msg.contains("loopback") || msg.contains("blocked"),
        "message should explain block reason, got {msg}",
    );
}

#[tokio::test]
async fn rejects_client_supplied_link_local_metadata_endpoint() {
    let strict = KnotProxyConfig {
        allow_private_hosts: false,
        ..test_config()
    };
    let h = Harness::with_config(strict).await;
    let resp = h
        .call(&format!(
            "/xrpc/sh.tangled.knot.version?knot={}",
            enc("http://169.254.169.254"),
        ))
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_value(resp).await;
    assert_eq!(v["error"], "InvalidRequest");
}

#[tokio::test]
async fn record_with_private_knot_returns_invalid_record() {
    let strict = KnotProxyConfig {
        allow_private_hosts: false,
        ..test_config()
    };
    let h = Harness::with_config(strict).await;
    let owner = did("did:plc:abalone");
    let rk = rkey("r1");
    let record = json!({
        "$type": "sh.tangled.repo",
        "createdAt": "2026-05-01T00:00:00Z",
        "knot": "http://10.0.0.5:3000",
        "name": "barnacle",
    });
    let uri = format!("at://{}/sh.tangled.repo/{}", owner.as_ref(), rk.as_ref());
    Mock::given(method("GET"))
        .and(path("/xrpc/com.atproto.repo.getRecord"))
        .and(query_param("repo", owner.as_ref()))
        .and(query_param("collection", "sh.tangled.repo"))
        .and(query_param("rkey", rk.as_ref()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uri": uri,
            "cid": CID,
            "value": record,
        })))
        .mount(&h.slingshot)
        .await;
    let resp = h
        .call(&format!(
            "/xrpc/sh.tangled.repo.blob?repo={}&path=x",
            enc(&uri),
        ))
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let v = body_value(resp).await;
    assert_eq!(v["error"], "InvalidRecord");
}

#[tokio::test]
async fn strips_basic_auth_from_credentialed_knot_url() {
    let h = Harness::new().await;
    let parsed = Url::parse(&h.knot.uri()).unwrap();
    let knot_with_creds = format!(
        "{}://attacker:secret@{}:{}/",
        parsed.scheme(),
        parsed.host_str().unwrap(),
        parsed.port().unwrap(),
    );
    let owner = did("did:plc:abalone");
    let rk = rkey("r1");
    let record = json!({
        "$type": "sh.tangled.repo",
        "createdAt": "2026-05-01T00:00:00Z",
        "knot": knot_with_creds,
        "name": "barnacle",
    });
    let uri = format!("at://{}/sh.tangled.repo/{}", owner.as_ref(), rk.as_ref());
    Mock::given(method("GET"))
        .and(path("/xrpc/com.atproto.repo.getRecord"))
        .and(query_param("repo", owner.as_ref()))
        .and(query_param("collection", "sh.tangled.repo"))
        .and(query_param("rkey", rk.as_ref()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uri": uri,
            "cid": CID,
            "value": record,
        })))
        .mount(&h.slingshot)
        .await;
    Mock::given(method("GET"))
        .and(path("/xrpc/sh.tangled.repo.blob"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(r#"{"path":"x"}"#, "application/json"),
        )
        .mount(&h.knot)
        .await;
    let target = format!("/xrpc/sh.tangled.repo.blob?repo={}&path=x", enc(&uri));
    let resp = h.call(&target).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let received = h.knot.received_requests().await.unwrap();
    let knot_call = received
        .iter()
        .find(|r| r.url.path() == "/xrpc/sh.tangled.repo.blob")
        .expect("knot received the proxied call");
    assert!(
        knot_call.headers.get("authorization").is_none(),
        "userinfo in knot field must not become an Authorization header",
    );
}

#[tokio::test]
async fn knot_redirect_surfaces_as_upstream_failed() {
    let h = Harness::new().await;
    let secondary = MockServer::start().await;
    h.mount_repo_record(&did("did:plc:abalone"), &rkey("r1"), "barnacle")
        .await;
    Mock::given(method("GET"))
        .and(path("/xrpc/sh.tangled.repo.blob"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", &format!("{}/secret", secondary.uri())),
        )
        .mount(&h.knot)
        .await;
    Mock::given(method("GET"))
        .and(path("/secret"))
        .respond_with(ResponseTemplate::new(200).set_body_string("leaked"))
        .mount(&secondary)
        .await;
    let resp = h
        .call(&format!(
            "/xrpc/sh.tangled.repo.blob?repo={}&path=x",
            enc("at://did:plc:abalone/sh.tangled.repo/r1"),
        ))
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let v = body_value(resp).await;
    assert_eq!(v["error"], "UpstreamFailed");
    let received = secondary.received_requests().await.unwrap();
    assert!(received.is_empty(), "redirect target must not be dialled");
}

#[tokio::test]
async fn forwards_repeated_query_params() {
    let h = Harness::new().await;
    h.mount_repo_record(&did("did:plc:limpet"), &rkey("r4"), "kelp")
        .await;
    Mock::given(method("GET"))
        .and(path("/xrpc/sh.tangled.repo.tags"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(r#"{"tags":[]}"#, "application/json"))
        .mount(&h.knot)
        .await;
    let target = format!(
        "/xrpc/sh.tangled.repo.tags?repo={}&filter=alpha&filter=beta",
        enc("at://did:plc:limpet/sh.tangled.repo/r4"),
    );
    let resp = h.call(&target).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let received = h.knot.received_requests().await.unwrap();
    let knot_call = received
        .iter()
        .find(|r| r.url.path() == "/xrpc/sh.tangled.repo.tags")
        .expect("knot received the proxied call");
    let filters: Vec<String> = knot_call
        .url
        .query_pairs()
        .filter(|(k, _)| k == "filter")
        .map(|(_, v)| v.into_owned())
        .collect();
    assert_eq!(filters, vec!["alpha".to_owned(), "beta".to_owned()]);
}

#[tokio::test]
async fn duplicate_repo_param_rejected_as_invalid_request() {
    let h = Harness::new().await;
    let target = format!(
        "/xrpc/sh.tangled.repo.blob?repo={}&repo={}",
        enc("at://did:plc:abalone/sh.tangled.repo/r1"),
        enc("at://did:plc:limpet/sh.tangled.repo/r2"),
    );
    let resp = h.call(&target).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_value(resp).await;
    assert_eq!(v["error"], "InvalidRequest");
    assert!(
        v["message"]
            .as_str()
            .unwrap_or_default()
            .contains("repo parameter must appear at most once"),
        "got {v}",
    );
}

#[tokio::test]
async fn duplicate_knot_param_rejected_as_invalid_request() {
    let h = Harness::new().await;
    let target = format!(
        "/xrpc/sh.tangled.knot.version?knot={}&knot={}",
        enc("https://oyster.cafe"),
        enc("https://nel.pet"),
    );
    let resp = h.call(&target).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_value(resp).await;
    assert_eq!(v["error"], "InvalidRequest");
}

#[tokio::test]
async fn rejects_client_supplied_plaintext_when_https_required() {
    let strict = KnotProxyConfig {
        require_https: true,
        ..test_config()
    };
    let h = Harness::with_config(strict).await;
    let resp = h
        .call(&format!(
            "/xrpc/sh.tangled.knot.version?knot={}",
            enc("http://oyster.cafe"),
        ))
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_value(resp).await;
    assert_eq!(v["error"], "InvalidRequest");
    assert!(
        v["message"]
            .as_str()
            .unwrap_or_default()
            .contains("must be https"),
        "got {v}",
    );
}

#[tokio::test]
async fn record_with_plaintext_knot_returns_invalid_record_when_https_required() {
    let strict = KnotProxyConfig {
        require_https: true,
        allow_private_hosts: true,
        ..test_config()
    };
    let h = Harness::with_config(strict).await;
    let owner = did("did:plc:abalone");
    let rk = rkey("r1");
    let record = json!({
        "$type": "sh.tangled.repo",
        "createdAt": "2026-05-01T00:00:00Z",
        "knot": "http://oyster.cafe",
        "name": "barnacle",
    });
    let uri = format!("at://{}/sh.tangled.repo/{}", owner.as_ref(), rk.as_ref());
    Mock::given(method("GET"))
        .and(path("/xrpc/com.atproto.repo.getRecord"))
        .and(query_param("repo", owner.as_ref()))
        .and(query_param("collection", "sh.tangled.repo"))
        .and(query_param("rkey", rk.as_ref()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uri": uri,
            "cid": CID,
            "value": record,
        })))
        .mount(&h.slingshot)
        .await;
    let resp = h
        .call(&format!(
            "/xrpc/sh.tangled.repo.blob?repo={}&path=x",
            enc(&uri),
        ))
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let v = body_value(resp).await;
    assert_eq!(v["error"], "InvalidRecord");
    assert!(
        v["message"]
            .as_str()
            .unwrap_or_default()
            .contains("requires https"),
        "got {v}",
    );
}

#[tokio::test]
async fn knot_not_modified_passes_through() {
    let h = Harness::new().await;
    h.mount_repo_record(&did("did:plc:limpet"), &rkey("r5"), "kelp")
        .await;
    Mock::given(method("GET"))
        .and(path("/xrpc/sh.tangled.repo.archive"))
        .respond_with(ResponseTemplate::new(304).insert_header("etag", "\"v1\""))
        .mount(&h.knot)
        .await;
    let target = format!(
        "/xrpc/sh.tangled.repo.archive?repo={}&ref=v1",
        enc("at://did:plc:limpet/sh.tangled.repo/r5"),
    );
    let resp = h
        .call_with_headers(&target, &[hdr("if-none-match", "\"v1\"")])
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(resp.headers().get("etag").unwrap(), "\"v1\"");
}
