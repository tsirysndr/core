use std::sync::Arc;

use axum::body::{Body, to_bytes};
use bobbin_edge_index::{CoverageWatch, EdgeStore, StateIndex};
use bobbin_knot_proxy::{KnotHttpConfig, KnotProxy, KnotProxyConfig};
use bobbin_record_lru::{CacheCapacity, LruRecordStore};
use bobbin_resolver::RepoIdResolver;
use bobbin_runtime::{RuntimeHasher, SystemClock};
use bobbin_search::{DEFAULT_WRITER_HEAP_BYTES, SearchIndex, SearchReader};
use bobbin_slingshot_client::SlingshotClient;
use bobbin_xrpc::{AppState, router};
use futures::stream::{self, StreamExt};
use http::{Request, StatusCode};
use jacquard_common::DefaultStr;
use jacquard_common::types::did::Did;
use jacquard_common::types::nsid::Nsid;
use jacquard_common::types::recordkey::Rkey;
use serde_json::{Value, json};
use tower::ServiceExt;
use url::Url;
use url::form_urlencoded::byte_serialize;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CID: &str = "bafyreieqygohnz2zqyvtvktbjpvhutphobcmbsnt4q5lc36ri7vpcmoz4i";

fn did(s: &str) -> Did<DefaultStr> {
    Did::new_owned(s).unwrap()
}

fn rkey(s: &str) -> Rkey<DefaultStr> {
    Rkey::new_owned(s).unwrap()
}

fn nsid(s: &'static str) -> Nsid<DefaultStr> {
    Nsid::new_static(s).unwrap()
}

async fn fresh_app(server_uri: &Url) -> AppState {
    AppState::new(
        Arc::new(LruRecordStore::new(CacheCapacity::from_bytes(64 * 1024))),
        SlingshotClient::with_default_http(server_uri.clone()).unwrap(),
        Arc::new(EdgeStore::new(RuntimeHasher::default())),
        Arc::new(StateIndex::new(RuntimeHasher::default())),
        Arc::new(StateIndex::new(RuntimeHasher::default())),
        Arc::new(CoverageWatch::new()),
        Arc::new(
            KnotProxy::new(
                KnotProxyConfig::default(),
                KnotHttpConfig::default(),
                Arc::new(SystemClock::new()),
                RuntimeHasher::default(),
            )
            .unwrap(),
        ),
        Arc::new(SearchIndex::new(DEFAULT_WRITER_HEAP_BYTES, Arc::new(SystemClock::new())).unwrap())
            as Arc<dyn SearchReader>,
        Arc::new(RepoIdResolver::detached(RuntimeHasher::default())),
    )
}

async fn mount_record(
    server: &MockServer,
    did: &Did<DefaultStr>,
    collection: &Nsid<DefaultStr>,
    rkey: &Rkey<DefaultStr>,
    value: Value,
) {
    let uri = format!(
        "at://{}/{}/{}",
        did.as_ref(),
        collection.as_ref(),
        rkey.as_ref()
    );
    let body = json!({ "uri": uri, "cid": CID, "value": value });
    Mock::given(method("GET"))
        .and(path("/xrpc/com.atproto.repo.getRecord"))
        .and(query_param("repo", did.as_ref()))
        .and(query_param("collection", collection.as_ref()))
        .and(query_param("rkey", rkey.as_ref()))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

fn xrpc_request(endpoint: &str, param: &str, value: &str) -> Request<Body> {
    Request::builder()
        .uri(format!("/xrpc/{endpoint}?{param}={value}"))
        .body(Body::empty())
        .unwrap()
}

fn xrpc_request_escaped(endpoint: &str, param: &str, value: &str) -> Request<Body> {
    let encoded: String = byte_serialize(value.as_bytes()).collect();
    Request::builder()
        .uri(format!("/xrpc/{endpoint}?{param}={encoded}"))
        .body(Body::empty())
        .unwrap()
}

async fn json_response(resp: axum::response::Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let parsed: Value = serde_json::from_slice(&bytes).expect("response is JSON");
    (status, parsed)
}

#[tokio::test]
async fn cold_start_serves_all_four_point_lookups() {
    let server = MockServer::start().await;
    let clam = did("did:plc:clam");

    mount_record(
        &server,
        &clam,
        &nsid("sh.tangled.repo"),
        &rkey("r1"),
        json!({
            "$type": "sh.tangled.repo",
            "name": "clam",
            "knot": "oyster.cafe",
            "createdAt": "2026-05-01T00:00:00Z"
        }),
    )
    .await;

    mount_record(
        &server,
        &clam,
        &nsid("sh.tangled.actor.profile"),
        &rkey("self"),
        json!({
            "$type": "sh.tangled.actor.profile",
            "bluesky": false,
            "description": "clam shell"
        }),
    )
    .await;

    mount_record(
        &server,
        &clam,
        &nsid("sh.tangled.repo.issue"),
        &rkey("i1"),
        json!({
            "$type": "sh.tangled.repo.issue",
            "repo": "did:plc:limpet",
            "title": "broken",
            "createdAt": "2026-05-01T00:00:00Z"
        }),
    )
    .await;

    mount_record(
        &server,
        &clam,
        &nsid("sh.tangled.repo.pull"),
        &rkey("p1"),
        json!({
            "$type": "sh.tangled.repo.pull",
            "title": "ship",
            "createdAt": "2026-05-01T00:00:00Z",
            "rounds": [],
            "target": {"repo": "did:plc:limpet", "branch": "main"}
        }),
    )
    .await;

    let state = fresh_app(&Url::parse(&server.uri()).unwrap()).await;
    let app = router(state);

    let cases = [
        (
            "sh.tangled.repo.getRepo",
            "repo",
            format!("at://{}/sh.tangled.repo/r1", clam.as_ref()),
            "knot",
            json!("oyster.cafe"),
        ),
        (
            "sh.tangled.actor.getProfile",
            "actor",
            format!("at://{}/sh.tangled.actor.profile/self", clam.as_ref()),
            "description",
            json!("clam shell"),
        ),
        (
            "sh.tangled.repo.getIssue",
            "issue",
            format!("at://{}/sh.tangled.repo.issue/i1", clam.as_ref()),
            "title",
            json!("broken"),
        ),
        (
            "sh.tangled.repo.getPull",
            "pull",
            format!("at://{}/sh.tangled.repo.pull/p1", clam.as_ref()),
            "title",
            json!("ship"),
        ),
    ];

    stream::iter(cases)
        .for_each(|(endpoint, param, at_uri, field, expected)| {
            let app = app.clone();
            async move {
                let resp = app
                    .oneshot(xrpc_request(endpoint, param, &at_uri))
                    .await
                    .unwrap();
                let (status, body) = json_response(resp).await;
                assert_eq!(status, StatusCode::OK, "{endpoint} status");
                assert_eq!(body["uri"], at_uri, "{endpoint} uri");
                assert_eq!(body["cid"], CID, "{endpoint} cid");
                assert_eq!(
                    body["value"][field], expected,
                    "{endpoint} body field {field}"
                );
            }
        })
        .await;
}

#[tokio::test]
async fn percent_escaped_at_uri_resolves_identically_to_raw() {
    let server = MockServer::start().await;
    let clam = did("did:plc:clam");
    mount_record(
        &server,
        &clam,
        &nsid("sh.tangled.actor.profile"),
        &rkey("self"),
        json!({
            "$type": "sh.tangled.actor.profile",
            "bluesky": false,
            "description": "clam shell"
        }),
    )
    .await;

    let state = fresh_app(&Url::parse(&server.uri()).unwrap()).await;
    let app = router(state);

    let at_uri = format!("at://{}/sh.tangled.actor.profile/self", clam.as_ref());

    let (raw_status, raw_body) = json_response(
        app.clone()
            .oneshot(xrpc_request(
                "sh.tangled.actor.getProfile",
                "actor",
                &at_uri,
            ))
            .await
            .unwrap(),
    )
    .await;
    let (escaped_status, escaped_body) = json_response(
        app.oneshot(xrpc_request_escaped(
            "sh.tangled.actor.getProfile",
            "actor",
            &at_uri,
        ))
        .await
        .unwrap(),
    )
    .await;

    assert_eq!(raw_status, StatusCode::OK, "raw at-uri status");
    assert_eq!(escaped_status, StatusCode::OK, "escaped at-uri status");
    assert_eq!(
        raw_body, escaped_body,
        "raw and escaped must resolve identically"
    );
    assert_eq!(escaped_body["uri"], at_uri);
}

#[tokio::test]
async fn second_call_is_served_from_lru() {
    let server = MockServer::start().await;
    let uni = did("did:plc:uni");
    let mock = Mock::given(method("GET"))
        .and(path("/xrpc/com.atproto.repo.getRecord"))
        .and(query_param("repo", uni.as_ref()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uri": format!("at://{}/sh.tangled.repo/r1", uni.as_ref()),
            "cid": CID,
            "value": {
                "$type": "sh.tangled.repo",
                "name": "uni",
                "knot": "witchcraft.systems",
                "createdAt": "2026-05-01T00:00:00Z"
            }
        })))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    let state = fresh_app(&Url::parse(&server.uri()).unwrap()).await;
    let app = router(state);
    let req_uri = format!("at://{}/sh.tangled.repo/r1", uni.as_ref());

    stream::iter(0..3)
        .for_each(|_| {
            let app = app.clone();
            let req_uri = req_uri.clone();
            async move {
                let resp = app
                    .oneshot(xrpc_request("sh.tangled.repo.getRepo", "repo", &req_uri))
                    .await
                    .unwrap();
                assert_eq!(resp.status(), StatusCode::OK);
            }
        })
        .await;

    drop(mock);
}

#[tokio::test]
async fn collection_mismatch_is_400() {
    let server = MockServer::start().await;
    let state = fresh_app(&Url::parse(&server.uri()).unwrap()).await;
    let app = router(state);
    let resp = app
        .oneshot(xrpc_request(
            "sh.tangled.repo.getRepo",
            "repo",
            "at://did:plc:clam/sh.tangled.actor.profile/self",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn handle_authority_is_400() {
    let server = MockServer::start().await;
    let state = fresh_app(&Url::parse(&server.uri()).unwrap()).await;
    let app = router(state);
    let resp = app
        .oneshot(xrpc_request(
            "sh.tangled.repo.getRepo",
            "repo",
            "at://witchcraft.systems/sh.tangled.repo/r1",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn slingshot_404_propagates_as_404() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/xrpc/com.atproto.repo.getRecord"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let state = fresh_app(&Url::parse(&server.uri()).unwrap()).await;
    let app = router(state);
    let resp = app
        .oneshot(xrpc_request(
            "sh.tangled.repo.getRepo",
            "repo",
            "at://did:plc:clam/sh.tangled.repo/missing",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn wrong_record_type_is_502() {
    let server = MockServer::start().await;
    mount_record(
        &server,
        &did("did:plc:clam"),
        &nsid("sh.tangled.repo"),
        &rkey("r1"),
        json!({
            "$type": "sh.tangled.knot",
            "knot": "oyster.cafe",
            "createdAt": "2026-05-01T00:00:00Z"
        }),
    )
    .await;
    let state = fresh_app(&Url::parse(&server.uri()).unwrap()).await;
    let app = router(state);
    let resp = app
        .oneshot(xrpc_request(
            "sh.tangled.repo.getRepo",
            "repo",
            "at://did:plc:clam/sh.tangled.repo/r1",
        ))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["error"], "InvalidRecord");
}

#[tokio::test]
async fn wrong_type_does_not_poison_cache() {
    let server = MockServer::start().await;
    let mock = Mock::given(method("GET"))
        .and(path("/xrpc/com.atproto.repo.getRecord"))
        .and(query_param("repo", "did:plc:clam"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uri": "at://did:plc:clam/sh.tangled.repo/r1",
            "cid": CID,
            "value": {
                "$type": "sh.tangled.knot",
                "knot": "oyster.cafe",
                "createdAt": "2026-05-01T00:00:00Z"
            }
        })))
        .expect(2)
        .mount_as_scoped(&server)
        .await;
    let state = fresh_app(&Url::parse(&server.uri()).unwrap()).await;
    let app = router(state);
    let req = || {
        xrpc_request(
            "sh.tangled.repo.getRepo",
            "repo",
            "at://did:plc:clam/sh.tangled.repo/r1",
        )
    };
    let first = app.clone().oneshot(req()).await.unwrap();
    assert_eq!(first.status(), StatusCode::BAD_GATEWAY);
    let second = app.clone().oneshot(req()).await.unwrap();
    assert_eq!(second.status(), StatusCode::BAD_GATEWAY);
    drop(mock);
}

#[tokio::test]
async fn profile_with_empty_preferred_handle_is_tolerated() {
    let server = MockServer::start().await;
    let nel = did("did:plc:nel");
    mount_record(
        &server,
        &nel,
        &nsid("sh.tangled.actor.profile"),
        &rkey("self"),
        json!({
            "$type": "sh.tangled.actor.profile",
            "bluesky": true,
            "preferredHandle": "",
            "description": "empty handle, valid profile"
        }),
    )
    .await;
    let state = fresh_app(&Url::parse(&server.uri()).unwrap()).await;
    let app = router(state);
    let at_uri = format!("at://{}/sh.tangled.actor.profile/self", nel.as_ref());
    let resp = app
        .oneshot(xrpc_request(
            "sh.tangled.actor.getProfile",
            "actor",
            &at_uri,
        ))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK, "status: {body}");
    assert_eq!(body["uri"], at_uri);
    assert_eq!(body["value"]["description"], "empty handle, valid profile");
    assert!(body["value"]["preferredHandle"].is_null());
}

#[tokio::test]
async fn missing_uri_param_returns_json_envelope() {
    let server = MockServer::start().await;
    let state = fresh_app(&Url::parse(&server.uri()).unwrap()).await;
    let app = router(state);
    let req = Request::builder()
        .uri("/xrpc/sh.tangled.repo.getRepo")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "InvalidRequest");
    assert!(body["message"].is_string());
}

#[tokio::test]
async fn malformed_at_uri_returns_400_envelope() {
    let server = MockServer::start().await;
    let state = fresh_app(&Url::parse(&server.uri()).unwrap()).await;
    let app = router(state);
    let resp = app
        .oneshot(xrpc_request(
            "sh.tangled.repo.getRepo",
            "repo",
            "definitely-not-an-at-uri",
        ))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "InvalidRequest");
}

#[tokio::test]
async fn upstream_uri_mismatch_routes_to_invalid_record() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/xrpc/com.atproto.repo.getRecord"))
        .and(query_param("repo", "did:plc:clam"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uri": "at://did:plc:limpet/sh.tangled.repo/elsewhere",
            "cid": CID,
            "value": {
                "$type": "sh.tangled.repo",
                "knot": "oyster.cafe",
                "createdAt": "2026-05-01T00:00:00Z"
            }
        })))
        .mount(&server)
        .await;
    let state = fresh_app(&Url::parse(&server.uri()).unwrap()).await;
    let app = router(state);
    let resp = app
        .oneshot(xrpc_request(
            "sh.tangled.repo.getRepo",
            "repo",
            "at://did:plc:clam/sh.tangled.repo/r1",
        ))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["error"], "InvalidRecord");
}

#[tokio::test]
async fn upstream_garbage_cid_routes_to_invalid_record() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/xrpc/com.atproto.repo.getRecord"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uri": "at://did:plc:clam/sh.tangled.repo/r1",
            "cid": "not-a-real-cid",
            "value": {
                "$type": "sh.tangled.repo",
                "knot": "oyster.cafe",
                "createdAt": "2026-05-01T00:00:00Z"
            }
        })))
        .mount(&server)
        .await;
    let state = fresh_app(&Url::parse(&server.uri()).unwrap()).await;
    let app = router(state);
    let resp = app
        .oneshot(xrpc_request(
            "sh.tangled.repo.getRepo",
            "repo",
            "at://did:plc:clam/sh.tangled.repo/r1",
        ))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["error"], "InvalidRecord");
}

#[tokio::test]
async fn oversize_upstream_body_routes_to_upstream_failed() {
    let server = MockServer::start().await;
    let payload = vec![b'x'; 8 * 1024 * 1024];
    Mock::given(method("GET"))
        .and(path("/xrpc/com.atproto.repo.getRecord"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_bytes(payload),
        )
        .mount(&server)
        .await;
    let state = fresh_app(&Url::parse(&server.uri()).unwrap()).await;
    let app = router(state);
    let resp = app
        .oneshot(xrpc_request(
            "sh.tangled.repo.getRepo",
            "repo",
            "at://did:plc:clam/sh.tangled.repo/r1",
        ))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["error"], "UpstreamFailed");
}

#[tokio::test]
async fn upstream_503_routes_to_upstream_failed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/xrpc/com.atproto.repo.getRecord"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let state = fresh_app(&Url::parse(&server.uri()).unwrap()).await;
    let app = router(state);
    let resp = app
        .oneshot(xrpc_request(
            "sh.tangled.repo.getRepo",
            "repo",
            "at://did:plc:clam/sh.tangled.repo/r1",
        ))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["error"], "UpstreamFailed");
}

#[tokio::test]
async fn get_repo_by_repo_did_returns_observed_record() {
    let server = MockServer::start().await;
    let owner_did = did("did:plc:scallop");
    let rk = rkey("r1");
    let repo_did = did("did:plc:limpet");
    mount_record(
        &server,
        &owner_did,
        &nsid("sh.tangled.repo"),
        &rk,
        json!({
            "$type": "sh.tangled.repo",
            "name": "scallop",
            "knot": "oyster.cafe",
            "createdAt": "2026-05-01T00:00:00Z",
            "repoDid": repo_did.as_ref(),
        }),
    )
    .await;

    let state = fresh_app(&Url::parse(&server.uri()).unwrap()).await;
    state
        .resolver
        .observe(owner_did.clone(), rk.clone(), Some(repo_did.clone()))
        .await;

    let app = router(state);
    let resp = app
        .oneshot(xrpc_request(
            "sh.tangled.repo.getRepoByRepoDid",
            "repoDid",
            repo_did.as_ref(),
        ))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["uri"],
        format!(
            "at://{}/sh.tangled.repo/{}",
            owner_did.as_ref(),
            rk.as_ref()
        )
    );
    assert_eq!(body["value"]["name"], "scallop");
    assert_eq!(body["value"]["repoDid"], repo_did.as_ref());
}

#[tokio::test]
async fn get_repo_by_repo_did_404_when_unobserved() {
    let server = MockServer::start().await;
    let state = fresh_app(&Url::parse(&server.uri()).unwrap()).await;
    let app = router(state);
    let resp = app
        .oneshot(xrpc_request(
            "sh.tangled.repo.getRepoByRepoDid",
            "repoDid",
            "did:plc:whelk",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_repo_by_repo_did_400_on_invalid_did() {
    let server = MockServer::start().await;
    let state = fresh_app(&Url::parse(&server.uri()).unwrap()).await;
    let app = router(state);
    let resp = app
        .oneshot(xrpc_request(
            "sh.tangled.repo.getRepoByRepoDid",
            "repoDid",
            "not-a-did",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn record_values_serialize_a_single_type_key() {
    let server = MockServer::start().await;

    mount_record(
        &server,
        &did("did:plc:teq"),
        &nsid("sh.tangled.repo"),
        &rkey("r1"),
        json!({
            "$type": "sh.tangled.repo",
            "name": "clam",
            "knot": "oyster.cafe",
            "createdAt": "2026-05-01T00:00:00Z"
        }),
    )
    .await;

    let app = router(fresh_app(&Url::parse(&server.uri()).unwrap()).await);
    let resp = app
        .oneshot(xrpc_request(
            "sh.tangled.repo.getRepo",
            "repo",
            "at://did:plc:teq/sh.tangled.repo/r1",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let raw = String::from_utf8(bytes.to_vec()).unwrap();
    assert_eq!(raw.matches("\"$type\"").count(), 1, "body: {raw}");
    assert!(raw.contains("\"$type\":\"sh.tangled.repo\""), "body: {raw}");
}
