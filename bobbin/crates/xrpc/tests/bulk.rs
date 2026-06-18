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
use http::{Request, StatusCode};
use jacquard_common::DefaultStr;
use jacquard_common::types::did::Did;
use jacquard_common::types::handle::Handle;
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

fn handle(s: &str) -> Handle<DefaultStr> {
    Handle::new_owned(s).unwrap()
}

struct Harness {
    server: MockServer,
    state: AppState,
}

impl Harness {
    async fn new() -> Self {
        let server = MockServer::start().await;
        let coverage = Arc::new(CoverageWatch::new());
        let state = AppState::new(
            Arc::new(LruRecordStore::new(CacheCapacity::from_bytes(64 * 1024))),
            SlingshotClient::with_default_http(Url::parse(&server.uri()).unwrap()).unwrap(),
            Arc::new(EdgeStore::new(RuntimeHasher::default())),
            Arc::new(StateIndex::new(RuntimeHasher::default())),
            Arc::new(StateIndex::new(RuntimeHasher::default())),
            coverage,
            Arc::new(
                KnotProxy::new(
                    KnotProxyConfig::default(),
                    KnotHttpConfig::default(),
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
        Self { server, state }
    }

    async fn mount(
        &self,
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
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.repo.getRecord"))
            .and(query_param("repo", did.as_ref()))
            .and(query_param("collection", collection.as_ref()))
            .and(query_param("rkey", rkey.as_ref()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "uri": uri,
                "cid": CID,
                "value": value,
            })))
            .mount(&self.server)
            .await;
    }

    async fn mount_404(
        &self,
        did: &Did<DefaultStr>,
        collection: &Nsid<DefaultStr>,
        rkey: &Rkey<DefaultStr>,
    ) {
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.repo.getRecord"))
            .and(query_param("repo", did.as_ref()))
            .and(query_param("collection", collection.as_ref()))
            .and(query_param("rkey", rkey.as_ref()))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(json!({"error": "RecordNotFound", "message": "missing"})),
            )
            .mount(&self.server)
            .await;
    }
}

fn enc(s: &str) -> String {
    byte_serialize(s.as_bytes()).collect()
}

fn bulk_request(endpoint: &str, key: &str, values: &[&str]) -> Request<Body> {
    let qs = values
        .iter()
        .map(|v| format!("{key}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    Request::builder()
        .uri(format!("/xrpc/{endpoint}?{qs}"))
        .body(Body::empty())
        .unwrap()
}

fn bulk_request_escaped(endpoint: &str, key: &str, values: &[&str]) -> Request<Body> {
    let qs = values
        .iter()
        .map(|v| format!("{key}={}", enc(v)))
        .collect::<Vec<_>>()
        .join("&");
    Request::builder()
        .uri(format!("/xrpc/{endpoint}?{qs}"))
        .body(Body::empty())
        .unwrap()
}

async fn json_response(resp: axum::response::Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let parsed: Value = serde_json::from_slice(&bytes).expect("JSON body");
    (status, parsed)
}

fn issue_body(repo_did: &Did<DefaultStr>, title: &str) -> Value {
    json!({
        "$type": "sh.tangled.repo.issue",
        "repo": repo_did.as_ref(),
        "title": title,
        "createdAt": "2026-05-01T00:00:00Z"
    })
}

fn pull_body(target_repo: &Did<DefaultStr>, title: &str) -> Value {
    json!({
        "$type": "sh.tangled.repo.pull",
        "title": title,
        "createdAt": "2026-05-01T00:00:00Z",
        "rounds": [],
        "target": {
            "branch": "main",
            "repo": target_repo.as_ref()
        }
    })
}

fn repo_body(name: &str) -> Value {
    json!({
        "$type": "sh.tangled.repo",
        "name": name,
        "knot": "oyster.cafe",
        "createdAt": "2026-05-01T00:00:00Z"
    })
}

fn profile_body(handle: &Handle<DefaultStr>) -> Value {
    json!({
        "$type": "sh.tangled.actor.profile",
        "bluesky": false,
        "preferredHandle": handle.as_ref()
    })
}

#[tokio::test]
async fn get_repos_returns_all_resolved_records() {
    let h = Harness::new().await;
    h.mount(
        &did("did:plc:nel"),
        &nsid("sh.tangled.repo"),
        &rkey("abalone"),
        repo_body("abalone"),
    )
    .await;
    h.mount(
        &did("did:plc:teq"),
        &nsid("sh.tangled.repo"),
        &rkey("limpet"),
        repo_body("limpet"),
    )
    .await;
    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(bulk_request(
            "sh.tangled.repo.getRepos",
            "repos",
            &[
                "at://did:plc:nel/sh.tangled.repo/abalone",
                "at://did:plc:teq/sh.tangled.repo/limpet",
            ],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let names: Vec<&str> = items
        .iter()
        .map(|v| v["value"]["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"abalone"));
    assert!(names.contains(&"limpet"));
}

#[tokio::test]
async fn get_profiles_returns_all_resolved_profiles() {
    let h = Harness::new().await;
    h.mount(
        &did("did:plc:nel"),
        &nsid("sh.tangled.actor.profile"),
        &rkey("self"),
        profile_body(&handle("witchcraft.systems")),
    )
    .await;
    h.mount(
        &did("did:plc:teq"),
        &nsid("sh.tangled.actor.profile"),
        &rkey("self"),
        profile_body(&handle("olaren.dev")),
    )
    .await;
    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(bulk_request(
            "sh.tangled.actor.getProfiles",
            "actors",
            &[
                "at://did:plc:nel/sh.tangled.actor.profile/self",
                "at://did:plc:teq/sh.tangled.actor.profile/self",
            ],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
}

#[tokio::test]
async fn get_profiles_accepts_percent_escaped_at_uris() {
    let h = Harness::new().await;
    h.mount(
        &did("did:plc:nel"),
        &nsid("sh.tangled.actor.profile"),
        &rkey("self"),
        profile_body(&handle("witchcraft.systems")),
    )
    .await;
    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(bulk_request_escaped(
            "sh.tangled.actor.getProfiles",
            "actors",
            &["at://did:plc:nel/sh.tangled.actor.profile/self"],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn get_issues_returns_all_resolved_issues() {
    let h = Harness::new().await;
    let repo = did("did:plc:abalone");
    h.mount(
        &did("did:plc:nel"),
        &nsid("sh.tangled.repo.issue"),
        &rkey("i1"),
        issue_body(&repo, "first"),
    )
    .await;
    h.mount(
        &did("did:plc:olaren"),
        &nsid("sh.tangled.repo.issue"),
        &rkey("i2"),
        issue_body(&repo, "second"),
    )
    .await;
    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(bulk_request(
            "sh.tangled.repo.getIssues",
            "issues",
            &[
                "at://did:plc:nel/sh.tangled.repo.issue/i1",
                "at://did:plc:olaren/sh.tangled.repo.issue/i2",
            ],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
}

#[tokio::test]
async fn get_pulls_returns_all_resolved_pulls() {
    let h = Harness::new().await;
    let target = did("did:plc:abalone");
    h.mount(
        &did("did:plc:nel"),
        &nsid("sh.tangled.repo.pull"),
        &rkey("p1"),
        pull_body(&target, "patch one"),
    )
    .await;
    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(bulk_request(
            "sh.tangled.repo.getPulls",
            "pulls",
            &["at://did:plc:nel/sh.tangled.repo.pull/p1"],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["value"]["title"], json!("patch one"));
}

#[tokio::test]
async fn missing_records_are_dropped_silently() {
    let h = Harness::new().await;
    h.mount(
        &did("did:plc:nel"),
        &nsid("sh.tangled.repo"),
        &rkey("abalone"),
        repo_body("abalone"),
    )
    .await;
    h.mount_404(
        &did("did:plc:teq"),
        &nsid("sh.tangled.repo"),
        &rkey("ghost"),
    )
    .await;
    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(bulk_request(
            "sh.tangled.repo.getRepos",
            "repos",
            &[
                "at://did:plc:nel/sh.tangled.repo/abalone",
                "at://did:plc:teq/sh.tangled.repo/ghost",
            ],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        1,
        "missing records must be dropped not fail the bulk call"
    );
    assert_eq!(items[0]["value"]["name"], json!("abalone"));
}

#[tokio::test]
async fn transient_failure_drops_only_that_record() {
    let h = Harness::new().await;
    h.mount(
        &did("did:plc:nel"),
        &nsid("sh.tangled.repo"),
        &rkey("conch"),
        repo_body("conch"),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/xrpc/com.atproto.repo.getRecord"))
        .and(query_param("repo", "did:plc:teq"))
        .and(query_param("collection", "sh.tangled.repo"))
        .and(query_param("rkey", "flaky"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.server)
        .await;
    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(bulk_request(
            "sh.tangled.repo.getRepos",
            "repos",
            &[
                "at://did:plc:nel/sh.tangled.repo/conch",
                "at://did:plc:teq/sh.tangled.repo/flaky",
            ],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        1,
        "transient upstream failure drops that record, it must not fail the bulk call"
    );
    assert_eq!(items[0]["value"]["name"], json!("conch"));
}

#[tokio::test]
async fn wrong_collection_uri_fails_bulk_request() {
    let h = Harness::new().await;
    h.mount(
        &did("did:plc:nel"),
        &nsid("sh.tangled.repo"),
        &rkey("conch"),
        repo_body("conch"),
    )
    .await;
    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(bulk_request(
            "sh.tangled.repo.getRepos",
            "repos",
            &[
                "at://did:plc:nel/sh.tangled.repo/conch",
                "at://did:plc:teq/sh.tangled.repo.issue/whelk",
            ],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a wrong-collection uri must fail the whole bulk request"
    );
    assert_eq!(body["error"], "InvalidRequest");
}

#[tokio::test]
async fn empty_uri_list_is_rejected() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/xrpc/sh.tangled.repo.getRepos")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn over_limit_uri_list_is_rejected() {
    let h = Harness::new().await;
    let uris: Vec<String> = (0..51)
        .map(|i| format!("at://did:plc:nel/sh.tangled.repo/r{i}"))
        .collect();
    let refs: Vec<&str> = uris.iter().map(|s| s.as_str()).collect();
    let app = router(h.state.clone());
    let resp = app
        .oneshot(bulk_request("sh.tangled.repo.getRepos", "repos", &refs))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn malformed_uri_in_list_returns_400() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let resp = app
        .oneshot(bulk_request(
            "sh.tangled.repo.getRepos",
            "repos",
            &["not-a-uri"],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
