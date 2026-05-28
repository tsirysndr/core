use std::sync::Arc;

use axum::body::{Body, to_bytes};
use bobbin_edge_index::{
    Coverage, CoverageWatch, EdgeStore, HydrantCursor, IssueStateKind, PageToken, PullStatusKind,
    StateIndex,
};
use bobbin_knot_proxy::{KnotHttpConfig, KnotProxy, KnotProxyConfig};
use bobbin_record_lru::{CacheCapacity, LruRecordStore};
use bobbin_resolver::RepoIdResolver;
use bobbin_runtime::{RuntimeHasher, SystemClock};
use bobbin_search::{DEFAULT_WRITER_HEAP_BYTES, SearchIndex, SearchReader};
use bobbin_slingshot_client::SlingshotClient;
use bobbin_types::edges::Edge;
use bobbin_types::ids::SubjectRef;
use bobbin_xrpc::{AppState, router};
use futures::stream::{self, StreamExt};
use http::{Request, StatusCode};
use jacquard_common::DefaultStr;
use jacquard_common::types::did::Did;
use jacquard_common::types::nsid::Nsid;
use jacquard_common::types::recordkey::Rkey;
use jacquard_common::types::string::AtUri;
use serde_json::{Value, json};
use tower::ServiceExt;
use url::Url;
use url::form_urlencoded::byte_serialize;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CID: &str = "bafyreieqygohnz2zqyvtvktbjpvhutphobcmbsnt4q5lc36ri7vpcmoz4i";

fn at(s: &str) -> AtUri<DefaultStr> {
    AtUri::new_owned(s).unwrap()
}

fn did(s: &str) -> Did<DefaultStr> {
    Did::new_owned(s).unwrap()
}

fn rkey(s: &str) -> Rkey<DefaultStr> {
    Rkey::new_owned(s).unwrap()
}

fn nsid(s: &'static str) -> Nsid<DefaultStr> {
    Nsid::new_static(s).unwrap()
}

fn subj(s: &str) -> SubjectRef {
    Did::<DefaultStr>::new_owned(s)
        .map(SubjectRef::Did)
        .unwrap_or_else(|_| SubjectRef::Uri(AtUri::new_owned(s).unwrap()))
}

struct Harness {
    server: MockServer,
    edges: Arc<EdgeStore>,
    coverage: Arc<CoverageWatch>,
    state: AppState,
}

static EDGE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_sort_micros() -> u64 {
    EDGE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl Harness {
    async fn new() -> Self {
        let server = MockServer::start().await;
        let edges = Arc::new(EdgeStore::new(RuntimeHasher::default()));
        let issue_states = Arc::new(StateIndex::new(RuntimeHasher::default()));
        let pull_statuses = Arc::new(StateIndex::new(RuntimeHasher::default()));
        let coverage = Arc::new(CoverageWatch::new());
        let state = AppState::new(
            Arc::new(LruRecordStore::new(CacheCapacity::from_bytes(64 * 1024))),
            SlingshotClient::with_default_http(Url::parse(&server.uri()).unwrap()).unwrap(),
            edges.clone(),
            issue_states.clone(),
            pull_statuses.clone(),
            coverage.clone(),
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
        Self {
            server,
            edges,
            coverage,
            state,
        }
    }

    fn add_edge(
        &self,
        kind: &Nsid<DefaultStr>,
        subject: &AtUri<DefaultStr>,
        source: &AtUri<DefaultStr>,
    ) {
        self.edges.add(Edge {
            kind: kind.clone(),
            subject: subj(subject.as_ref()),
            source: source.clone(),
            sort_micros: next_sort_micros(),
        });
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
        let body = json!({ "uri": uri, "cid": CID, "value": value });
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.repo.getRecord"))
            .and(query_param("repo", did.as_ref()))
            .and(query_param("collection", collection.as_ref()))
            .and(query_param("rkey", rkey.as_ref()))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&self.server)
            .await;
    }

    fn promote_ready(&self, events: u64, cursor: u64) {
        self.coverage.update(|_| Coverage::Ready {
            events_processed: events,
            last_cursor: HydrantCursor::new(cursor),
        });
    }

    fn warming(&self, events: u64, cursor: u64) {
        self.coverage.update(|_| Coverage::Warming {
            events_processed: events,
            last_cursor: HydrantCursor::new(cursor),
        });
    }
}

fn list_request(endpoint: &str, subject: &str, extras: &[(&str, &str)]) -> Request<Body> {
    let mut qs = format!("subject={}", encode(subject));
    extras.iter().for_each(|(k, v)| {
        qs.push('&');
        qs.push_str(k);
        qs.push('=');
        qs.push_str(&encode(v));
    });
    Request::builder()
        .uri(format!("/xrpc/{endpoint}?{qs}"))
        .body(Body::empty())
        .unwrap()
}

fn encode(s: &str) -> String {
    byte_serialize(s.as_bytes()).collect()
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

fn pull_body(repo_did: &Did<DefaultStr>, title: &str) -> Value {
    json!({
        "$type": "sh.tangled.repo.pull",
        "title": title,
        "createdAt": "2026-05-01T00:00:00Z",
        "rounds": [],
        "target": {
            "repo": repo_did.as_ref(),
            "branch": "main"
        }
    })
}

fn star_body(subject_did: &Did<DefaultStr>) -> Value {
    json!({
        "$type": "sh.tangled.feed.star",
        "createdAt": "2026-05-01T00:00:00Z",
        "subject": {
            "$type": "sh.tangled.feed.star#repo",
            "did": subject_did.as_ref()
        }
    })
}

fn follow_body(subject_did: &Did<DefaultStr>) -> Value {
    json!({
        "$type": "sh.tangled.graph.follow",
        "createdAt": "2026-05-01T00:00:00Z",
        "subject": subject_did.as_ref()
    })
}

#[tokio::test]
async fn list_issues_with_no_edges_returns_empty_items() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let resp = app
        .oneshot(list_request(
            "sh.tangled.repo.listIssues",
            "at://did:plc:abalone",
            &[],
        ))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"], json!([]));
    assert!(body["cursor"].is_null());
}

#[tokio::test]
async fn count_issues_with_no_edges_returns_zero() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let resp = app
        .oneshot(list_request(
            "sh.tangled.repo.countIssues",
            "at://did:plc:abalone",
            &[],
        ))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], json!(0));
    assert_eq!(body["distinctAuthors"], json!(0));
}

#[tokio::test]
async fn list_issues_hydrates_via_slingshot_when_edges_present() {
    let h = Harness::new().await;
    let repo = did("did:plc:abalone");
    let subject = at(&format!("at://{}", repo.as_ref()));
    let owners = [
        ("did:plc:nel", "i1", "first"),
        ("did:plc:olaren", "i2", "second"),
    ];
    stream::iter(owners)
        .for_each(|(d, r, title)| {
            let h = &h;
            let subject = subject.clone();
            let repo = repo.clone();
            async move {
                let d_did = did(d);
                let rk = rkey(r);
                h.add_edge(
                    &nsid("sh.tangled.repo.issue"),
                    &subject,
                    &at(&format!(
                        "at://{}/sh.tangled.repo.issue/{}",
                        d_did.as_ref(),
                        rk.as_ref()
                    )),
                );
                h.mount(
                    &d_did,
                    &nsid("sh.tangled.repo.issue"),
                    &rk,
                    issue_body(&repo, title),
                )
                .await;
            }
        })
        .await;

    let app = router(h.state.clone());
    let resp = app
        .oneshot(list_request(
            "sh.tangled.repo.listIssues",
            subject.as_ref(),
            &[],
        ))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2);
    let titles: Vec<&str> = items
        .iter()
        .map(|v| v["value"]["title"].as_str().unwrap())
        .collect();
    assert!(titles.contains(&"first"));
    assert!(titles.contains(&"second"));
    assert_eq!(items[0]["cid"], CID);
    assert!(items[0]["uri"].as_str().unwrap().starts_with("at://"));
}

#[tokio::test]
async fn count_distinct_authors_dedupes_per_author() {
    let h = Harness::new().await;
    let subject = at("at://did:plc:abalone");
    h.add_edge(
        &nsid("sh.tangled.feed.star"),
        &subject,
        &at("at://did:plc:nel/sh.tangled.feed.star/s1"),
    );
    h.add_edge(
        &nsid("sh.tangled.feed.star"),
        &subject,
        &at("at://did:plc:nel/sh.tangled.feed.star/s2"),
    );
    h.add_edge(
        &nsid("sh.tangled.feed.star"),
        &subject,
        &at("at://did:plc:olaren/sh.tangled.feed.star/s3"),
    );

    let app = router(h.state.clone());
    let resp = app
        .oneshot(list_request(
            "sh.tangled.feed.countStars",
            subject.as_ref(),
            &[],
        ))
        .await
        .unwrap();
    let (_, body) = json_response(resp).await;
    assert_eq!(body["count"], json!(3));
    assert_eq!(body["distinctAuthors"], json!(2));
}

#[tokio::test]
async fn list_items_stable_across_coverage_promotion() {
    let h = Harness::new().await;
    let subject = at("at://did:plc:abalone");
    let nel = did("did:plc:nel");
    h.add_edge(
        &nsid("sh.tangled.feed.star"),
        &subject,
        &at(&format!("at://{}/sh.tangled.feed.star/s1", nel.as_ref())),
    );
    h.mount(
        &nel,
        &nsid("sh.tangled.feed.star"),
        &rkey("s1"),
        star_body(&did("did:plc:abalone")),
    )
    .await;

    let app = router(h.state.clone());
    h.warming(1, 5);
    let (_, before) = json_response(
        app.clone()
            .oneshot(list_request(
                "sh.tangled.feed.listStars",
                subject.as_ref(),
                &[],
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(before["items"].as_array().unwrap().len(), 1);

    h.promote_ready(2, 9);
    let (_, after) = json_response(
        app.oneshot(list_request(
            "sh.tangled.feed.listStars",
            subject.as_ref(),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(
        after["items"].as_array().unwrap().len(),
        before["items"].as_array().unwrap().len(),
    );
    assert_eq!(after["items"], before["items"]);
}

#[tokio::test]
async fn list_paginates_via_cursor() {
    let h = Harness::new().await;
    let subject = at("at://did:plc:abalone");
    let repo = did("did:plc:abalone");
    let owners = [
        ("did:plc:nel", "i1"),
        ("did:plc:olaren", "i2"),
        ("did:plc:teq", "i3"),
        ("did:plc:lyna", "i4"),
        ("did:plc:bailey", "i5"),
    ];
    stream::iter(owners)
        .for_each(|(d, r)| {
            let h = &h;
            let subject = subject.clone();
            let repo = repo.clone();
            async move {
                let d_did = did(d);
                let rk = rkey(r);
                h.add_edge(
                    &nsid("sh.tangled.repo.issue"),
                    &subject,
                    &at(&format!(
                        "at://{}/sh.tangled.repo.issue/{}",
                        d_did.as_ref(),
                        rk.as_ref()
                    )),
                );
                h.mount(
                    &d_did,
                    &nsid("sh.tangled.repo.issue"),
                    &rk,
                    issue_body(&repo, &format!("issue-{}", rk.as_ref())),
                )
                .await;
            }
        })
        .await;

    let app = router(h.state.clone());
    let (_, page1) = json_response(
        app.clone()
            .oneshot(list_request(
                "sh.tangled.repo.listIssues",
                subject.as_ref(),
                &[("limit", "2")],
            ))
            .await
            .unwrap(),
    )
    .await;
    let page1_items = page1["items"].as_array().unwrap().clone();
    assert_eq!(page1_items.len(), 2);
    let cursor = page1["cursor"]
        .as_str()
        .expect("first page must yield a cursor")
        .to_owned();
    assert!(
        PageToken::decode_token(&cursor).is_ok(),
        "cursor must be a TID-shaped token"
    );

    let (_, page2) = json_response(
        app.oneshot(list_request(
            "sh.tangled.repo.listIssues",
            subject.as_ref(),
            &[("limit", "10"), ("cursor", &cursor)],
        ))
        .await
        .unwrap(),
    )
    .await;
    let page2_items = page2["items"].as_array().unwrap().clone();
    assert_eq!(page2_items.len(), 3);
    assert!(page2["cursor"].is_null(), "tail page must not promise more");

    let union: Vec<&str> = page1_items
        .iter()
        .chain(page2_items.iter())
        .map(|item| item["uri"].as_str().unwrap())
        .collect();
    assert_eq!(union.len(), owners.len(), "union covers every owner");
    let mut sorted = union.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), owners.len(), "no duplicates across pages");
}

#[tokio::test]
async fn pagination_unaffected_by_coverage_promotion() {
    let h = Harness::new().await;
    let subject = at("at://did:plc:abalone");
    let repo = did("did:plc:abalone");
    let owners = [("did:plc:nel", "i1"), ("did:plc:olaren", "i2")];
    stream::iter(owners)
        .for_each(|(d, r)| {
            let h = &h;
            let subject = subject.clone();
            let repo = repo.clone();
            async move {
                let d_did = did(d);
                let rk = rkey(r);
                h.add_edge(
                    &nsid("sh.tangled.repo.issue"),
                    &subject,
                    &at(&format!(
                        "at://{}/sh.tangled.repo.issue/{}",
                        d_did.as_ref(),
                        rk.as_ref()
                    )),
                );
                h.mount(
                    &d_did,
                    &nsid("sh.tangled.repo.issue"),
                    &rk,
                    issue_body(&repo, &format!("issue-{}", rk.as_ref())),
                )
                .await;
            }
        })
        .await;

    h.warming(1, 5);
    let app = router(h.state.clone());
    let (_, page1) = json_response(
        app.clone()
            .oneshot(list_request(
                "sh.tangled.repo.listIssues",
                subject.as_ref(),
                &[("limit", "1")],
            ))
            .await
            .unwrap(),
    )
    .await;
    let cursor = page1["cursor"].as_str().unwrap().to_owned();

    h.promote_ready(2, 9);
    let (_, page2) = json_response(
        app.oneshot(list_request(
            "sh.tangled.repo.listIssues",
            subject.as_ref(),
            &[("limit", "10"), ("cursor", &cursor)],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(page2["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn invalid_cursor_returns_400() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let resp = app
        .oneshot(list_request(
            "sh.tangled.repo.listIssues",
            "at://did:plc:abalone",
            &[("cursor", "not-a-number")],
        ))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "InvalidRequest");
}

#[tokio::test]
async fn list_follows_subject_is_followee_did() {
    let h = Harness::new().await;
    let followee = did("did:plc:bailey");
    let subject = at(&format!("at://{}", followee.as_ref()));
    h.add_edge(
        &nsid("sh.tangled.graph.follow"),
        &subject,
        &at("at://did:plc:nel/sh.tangled.graph.follow/f1"),
    );
    h.mount(
        &did("did:plc:nel"),
        &nsid("sh.tangled.graph.follow"),
        &rkey("f1"),
        follow_body(&followee),
    )
    .await;

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.graph.listFollows",
            subject.as_ref(),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["value"]["subject"], followee.as_ref());
}

#[tokio::test]
async fn upstream_failure_during_hydration_drops_only_that_item() {
    let h = Harness::new().await;
    let subject = at("at://did:plc:squid");
    let kind = nsid("sh.tangled.repo.issue");
    let repo = did("did:plc:squid");
    h.add_edge(
        &kind,
        &subject,
        &at("at://did:plc:nel/sh.tangled.repo.issue/ok"),
    );
    h.add_edge(
        &kind,
        &subject,
        &at("at://did:plc:teq/sh.tangled.repo.issue/flaky"),
    );
    h.mount(
        &did("did:plc:nel"),
        &kind,
        &rkey("ok"),
        issue_body(&repo, "kelp survey"),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/xrpc/com.atproto.repo.getRecord"))
        .and(query_param("repo", "did:plc:teq"))
        .and(query_param("collection", "sh.tangled.repo.issue"))
        .and(query_param("rkey", "flaky"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.server)
        .await;

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.repo.listIssues",
            subject.as_ref(),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1, "flaky item dropped, healthy sibling kept");
    assert_eq!(
        items[0]["uri"].as_str().unwrap(),
        "at://did:plc:nel/sh.tangled.repo.issue/ok",
    );
}

#[tokio::test]
async fn transient_failure_keeps_edge_so_count_stays_whole() {
    let h = Harness::new().await;
    let subject = at("at://did:plc:squid");
    let kind = nsid("sh.tangled.repo.issue");
    let repo = did("did:plc:squid");
    h.add_edge(
        &kind,
        &subject,
        &at("at://did:plc:nel/sh.tangled.repo.issue/ok"),
    );
    h.add_edge(
        &kind,
        &subject,
        &at("at://did:plc:teq/sh.tangled.repo.issue/flaky"),
    );
    h.mount(
        &did("did:plc:nel"),
        &kind,
        &rkey("ok"),
        issue_body(&repo, "kelp survey"),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/xrpc/com.atproto.repo.getRecord"))
        .and(query_param("repo", "did:plc:teq"))
        .and(query_param("collection", "sh.tangled.repo.issue"))
        .and(query_param("rkey", "flaky"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.server)
        .await;

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.clone()
            .oneshot(list_request(
                "sh.tangled.repo.listIssues",
                subject.as_ref(),
                &[],
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 1);

    let (cstatus, cbody) = json_response(
        app.oneshot(list_request(
            "sh.tangled.repo.countIssues",
            subject.as_ref(),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(cstatus, StatusCode::OK);
    assert_eq!(
        cbody["count"],
        json!(2),
        "a transient 503 must not evict the edge, count stays whole",
    );
}

#[tokio::test]
async fn gone_item_is_evicted_so_count_converges_to_list() {
    let h = Harness::new().await;
    let subject = at("at://did:plc:squid");
    let kind = nsid("sh.tangled.repo.issue");
    let repo = did("did:plc:squid");
    h.add_edge(
        &kind,
        &subject,
        &at("at://did:plc:nel/sh.tangled.repo.issue/ok"),
    );
    h.add_edge(
        &kind,
        &subject,
        &at("at://did:plc:teq/sh.tangled.repo.issue/gone"),
    );
    h.mount(
        &did("did:plc:nel"),
        &kind,
        &rkey("ok"),
        issue_body(&repo, "kelp survey"),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/xrpc/com.atproto.repo.getRecord"))
        .and(query_param("repo", "did:plc:teq"))
        .and(query_param("collection", "sh.tangled.repo.issue"))
        .and(query_param("rkey", "gone"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&h.server)
        .await;

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.clone()
            .oneshot(list_request(
                "sh.tangled.repo.listIssues",
                subject.as_ref(),
                &[],
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["items"].as_array().unwrap().len(),
        1,
        "gone item dropped from the page"
    );

    let (cstatus, cbody) = json_response(
        app.oneshot(list_request(
            "sh.tangled.repo.countIssues",
            subject.as_ref(),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(cstatus, StatusCode::OK);
    assert_eq!(
        cbody["count"],
        json!(1),
        "a definitive 404 must evict the dead edge so count matches the list",
    );
}

#[tokio::test]
async fn handle_authority_subject_is_400() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let cases = [
        "sh.tangled.feed.listStars",
        "sh.tangled.feed.countStars",
        "sh.tangled.graph.listFollows",
        "sh.tangled.graph.countFollows",
        "sh.tangled.repo.listIssues",
        "sh.tangled.repo.countIssues",
        "sh.tangled.repo.listPulls",
        "sh.tangled.repo.countPulls",
        "sh.tangled.feed.listComments",
        "sh.tangled.feed.countComments",
    ];
    stream::iter(cases)
        .for_each(|endpoint| {
            let app = app.clone();
            async move {
                let resp = app
                    .oneshot(list_request(endpoint, "at://oyster.cafe", &[]))
                    .await
                    .unwrap();
                let (status, body) = json_response(resp).await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{endpoint}");
                assert_eq!(body["error"], "InvalidRequest", "{endpoint}");
                assert!(
                    body["message"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("did, not a handle"),
                    "{endpoint}: {}",
                    body["message"]
                );
            }
        })
        .await;
}

#[tokio::test]
async fn empty_subject_is_400() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let resp = app
        .oneshot(list_request("sh.tangled.repo.listIssues", "", &[]))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "InvalidRequest");
}

#[tokio::test]
async fn limit_below_min_or_above_max_is_400() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let cases = [("0", "below"), ("1001", "above")];
    stream::iter(cases)
        .for_each(|(limit, label)| {
            let app = app.clone();
            async move {
                let resp = app
                    .oneshot(list_request(
                        "sh.tangled.repo.listIssues",
                        "at://did:plc:abalone",
                        &[("limit", limit)],
                    ))
                    .await
                    .unwrap();
                let (status, body) = json_response(resp).await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "limit {label}");
                assert_eq!(body["error"], "InvalidRequest", "limit {label}");
            }
        })
        .await;
}

#[tokio::test]
async fn count_after_remove_source_returns_zero() {
    let h = Harness::new().await;
    let subject = at("at://did:plc:abalone");
    let source = at("at://did:plc:nel/sh.tangled.feed.star/s1");
    h.add_edge(&nsid("sh.tangled.feed.star"), &subject, &source);
    h.edges.remove_source(&source);

    let app = router(h.state.clone());
    let (_, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.feed.countStars",
            subject.as_ref(),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(body["count"], json!(0));
    assert_eq!(body["distinctAuthors"], json!(0));
}

#[tokio::test]
async fn list_feed_comments_hydrates_end_to_end() {
    let h = Harness::new().await;
    let issue_uri = at("at://did:plc:abalone/sh.tangled.repo.issue/i1");
    let nel = did("did:plc:nel");
    let rk = rkey("c1");
    h.add_edge(
        &nsid("sh.tangled.feed.comment"),
        &issue_uri,
        &at(&format!(
            "at://{}/sh.tangled.feed.comment/{}",
            nel.as_ref(),
            rk.as_ref()
        )),
    );
    h.mount(
        &nel,
        &nsid("sh.tangled.feed.comment"),
        &rk,
        json!({
            "$type": "sh.tangled.feed.comment",
            "subject": { "uri": issue_uri.as_ref(), "cid": "bafkqaaa" },
            "body": { "$type": "sh.tangled.markup.markdown", "text": "thoughts" },
            "createdAt": "2026-05-01T00:00:00Z"
        }),
    )
    .await;

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.feed.listComments",
            issue_uri.as_ref(),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["value"]["body"]["text"], json!("thoughts"));
    assert_eq!(
        items[0]["value"]["subject"]["uri"],
        json!(issue_uri.as_ref())
    );
}

#[tokio::test]
async fn list_item_cid_is_present() {
    let h = Harness::new().await;
    let subject = at("at://did:plc:abalone");
    let nel = did("did:plc:nel");
    h.add_edge(
        &nsid("sh.tangled.feed.star"),
        &subject,
        &at(&format!("at://{}/sh.tangled.feed.star/s1", nel.as_ref())),
    );
    h.mount(
        &nel,
        &nsid("sh.tangled.feed.star"),
        &rkey("s1"),
        star_body(&did("did:plc:abalone")),
    )
    .await;

    let app = router(h.state.clone());
    let (_, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.feed.listStars",
            subject.as_ref(),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    let item = &body["items"][0];
    assert!(
        item.as_object().unwrap().contains_key("cid"),
        "list items must mirror getRecord output shape and include cid"
    );
    assert_eq!(item["cid"], json!(CID));
}

#[tokio::test]
async fn count_feed_comments_subjects_on_issue_uri() {
    let h = Harness::new().await;
    let issue_uri = at("at://did:plc:abalone/sh.tangled.repo.issue/i1");
    h.add_edge(
        &nsid("sh.tangled.feed.comment"),
        &issue_uri,
        &at("at://did:plc:nel/sh.tangled.feed.comment/c1"),
    );
    h.add_edge(
        &nsid("sh.tangled.feed.comment"),
        &issue_uri,
        &at("at://did:plc:olaren/sh.tangled.feed.comment/c2"),
    );

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.feed.countComments",
            issue_uri.as_ref(),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], json!(2));
    assert_eq!(body["distinctAuthors"], json!(2));
}

#[tokio::test]
async fn list_item_404_dropped_not_404_for_subject() {
    let h = Harness::new().await;
    let subject = at("at://did:plc:squid");
    let kind = nsid("sh.tangled.repo.issue");
    let repo = did("did:plc:squid");
    h.add_edge(
        &kind,
        &subject,
        &at("at://did:plc:nel/sh.tangled.repo.issue/live"),
    );
    h.add_edge(
        &kind,
        &subject,
        &at("at://did:plc:teq/sh.tangled.repo.issue/missing"),
    );
    h.mount(
        &did("did:plc:nel"),
        &kind,
        &rkey("live"),
        issue_body(&repo, "kelp survives"),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/xrpc/com.atproto.repo.getRecord"))
        .and(query_param("repo", "did:plc:teq"))
        .and(query_param("collection", "sh.tangled.repo.issue"))
        .and(query_param("rkey", "missing"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": "RecordNotFound",
            "message": "could not find record"
        })))
        .mount(&h.server)
        .await;

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.repo.listIssues",
            subject.as_ref(),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a stale-index 404 drops that item, it must not 404 or 502 the subject's list",
    );
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1, "stale 404 item dropped, live sibling kept");
    assert_eq!(
        items[0]["uri"].as_str().unwrap(),
        "at://did:plc:nel/sh.tangled.repo.issue/live",
    );
}

#[tokio::test]
async fn list_item_with_wrong_type_tag_dropped() {
    let h = Harness::new().await;
    let subject = at("at://did:plc:squid");
    let kind = nsid("sh.tangled.feed.star");
    h.add_edge(
        &kind,
        &subject,
        &at("at://did:plc:nel/sh.tangled.feed.star/good"),
    );
    h.add_edge(
        &kind,
        &subject,
        &at("at://did:plc:teq/sh.tangled.feed.star/wrong"),
    );
    h.mount(
        &did("did:plc:nel"),
        &kind,
        &rkey("good"),
        star_body(&did("did:plc:squid")),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/xrpc/com.atproto.repo.getRecord"))
        .and(query_param("repo", "did:plc:teq"))
        .and(query_param("collection", "sh.tangled.feed.star"))
        .and(query_param("rkey", "wrong"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uri": "at://did:plc:teq/sh.tangled.feed.star/wrong",
            "cid": CID,
            "value": {
                "$type": "sh.tangled.feed.reaction",
                "createdAt": "2026-05-01T00:00:00Z",
                "subject": "at://did:plc:squid"
            }
        })))
        .mount(&h.server)
        .await;

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.feed.listStars",
            subject.as_ref(),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1, "wrong-type item dropped, valid star kept");
    assert_eq!(
        items[0]["uri"].as_str().unwrap(),
        "at://did:plc:nel/sh.tangled.feed.star/good",
    );
}

#[tokio::test]
async fn list_item_with_mismatched_collection_dropped() {
    let h = Harness::new().await;
    let subject = at("at://did:plc:squid");
    let kind = nsid("sh.tangled.repo.issue");
    let repo = did("did:plc:squid");
    h.add_edge(
        &kind,
        &subject,
        &at("at://did:plc:nel/sh.tangled.repo.issue/live"),
    );
    h.add_edge(
        &kind,
        &subject,
        &at("at://did:plc:teq/sh.tangled.feed.star/whelk"),
    );
    h.mount(
        &did("did:plc:nel"),
        &kind,
        &rkey("live"),
        issue_body(&repo, "kelp survives"),
    )
    .await;

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.repo.listIssues",
            subject.as_ref(),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a mismatched-collection index edge must not 400 the subject's list",
    );
    let items = body["items"].as_array().expect("items array");
    assert_eq!(
        items.len(),
        1,
        "mismatched-collection edge dropped, live sibling kept"
    );
    assert_eq!(
        items[0]["uri"].as_str().unwrap(),
        "at://did:plc:nel/sh.tangled.repo.issue/live",
    );
}

#[tokio::test]
async fn bare_did_endpoints_reject_at_uri_subject() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let cases = [
        "sh.tangled.graph.listFollows",
        "sh.tangled.graph.countFollows",
    ];
    stream::iter(cases)
        .for_each(|endpoint| {
            let app = app.clone();
            async move {
                let resp = app
                    .oneshot(list_request(
                        endpoint,
                        "at://did:plc:abalone/sh.tangled.repo/r1",
                        &[],
                    ))
                    .await
                    .unwrap();
                let (status, body) = json_response(resp).await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{endpoint}");
                assert_eq!(body["error"], "InvalidRequest", "{endpoint}");
                assert!(
                    body["message"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("bare did"),
                    "{endpoint}: {}",
                    body["message"],
                );
            }
        })
        .await;
}

#[tokio::test]
async fn repo_pointing_endpoints_reject_at_uri_subject() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let cases = [
        "sh.tangled.repo.listIssues",
        "sh.tangled.repo.countIssues",
        "sh.tangled.repo.listPulls",
        "sh.tangled.repo.countPulls",
        "sh.tangled.repo.listArtifacts",
        "sh.tangled.repo.countArtifacts",
    ];
    stream::iter(cases)
        .for_each(|endpoint| {
            let app = app.clone();
            async move {
                let resp = app
                    .oneshot(list_request(
                        endpoint,
                        "at://did:plc:abalone/sh.tangled.repo/r1",
                        &[],
                    ))
                    .await
                    .unwrap();
                let (status, body) = json_response(resp).await;
                assert_eq!(
                    status,
                    StatusCode::BAD_REQUEST,
                    "{endpoint} must reject rkey-form subjects since rkeys are unstable; clients must send the repoDID",
                );
                assert!(
                    body["message"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("bare did"),
                    "{endpoint}: {}",
                    body["message"],
                );
            }
        })
        .await;
}

#[tokio::test]
async fn repo_pointing_endpoints_accept_bare_did() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let cases = [
        "sh.tangled.repo.listIssues",
        "sh.tangled.repo.countIssues",
        "sh.tangled.repo.listPulls",
        "sh.tangled.repo.countPulls",
        "sh.tangled.repo.listArtifacts",
        "sh.tangled.repo.countArtifacts",
    ];
    stream::iter(cases)
        .for_each(|endpoint| {
            let app = app.clone();
            async move {
                let resp = app
                    .oneshot(list_request(endpoint, "did:plc:abalone", &[]))
                    .await
                    .unwrap();
                let (status, _body) = json_response(resp).await;
                assert_eq!(status, StatusCode::OK, "{endpoint} must accept bare did");
            }
        })
        .await;
}

#[tokio::test]
async fn feed_comment_endpoints_reject_bare_did_or_wrong_collection() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let endpoints = [
        "sh.tangled.feed.listComments",
        "sh.tangled.feed.countComments",
    ];
    let inputs = [
        "at://did:plc:abalone",
        "at://did:plc:abalone/sh.tangled.repo/r1",
    ];
    let cases = endpoints
        .iter()
        .copied()
        .flat_map(|endpoint| inputs.iter().copied().map(move |input| (endpoint, input)));
    stream::iter(cases)
        .for_each(|(endpoint, input)| {
            let app = app.clone();
            async move {
                let resp = app
                    .oneshot(list_request(endpoint, input, &[]))
                    .await
                    .unwrap();
                let (status, body) = json_response(resp).await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{endpoint} input={input}");
                let msg = body["message"].as_str().unwrap_or_default();
                assert!(
                    msg.contains("sh.tangled.repo.issue") && msg.contains("sh.tangled.repo.pull"),
                    "{endpoint} input={input}: {msg}",
                );
            }
        })
        .await;
}

#[tokio::test]
async fn star_endpoints_reject_unrelated_collection() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let endpoints = ["sh.tangled.feed.listStars", "sh.tangled.feed.countStars"];
    stream::iter(endpoints)
        .for_each(|endpoint| {
            let app = app.clone();
            async move {
                let resp = app
                    .oneshot(list_request(
                        endpoint,
                        "at://did:plc:abalone/sh.tangled.knot/k1",
                        &[],
                    ))
                    .await
                    .unwrap();
                let (status, body) = json_response(resp).await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{endpoint}");
                let msg = body["message"].as_str().unwrap_or_default();
                assert!(msg.contains("sh.tangled.string"), "{endpoint}: {msg}",);
            }
        })
        .await;
}

#[tokio::test]
async fn star_endpoints_reject_repo_uri_subject() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let resp = app
        .oneshot(list_request(
            "sh.tangled.feed.countStars",
            "at://did:plc:abalone/sh.tangled.repo/r1",
            &[],
        ))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "rkey-form repo URI must be rejected; clients must send the repoDID directly",
    );
    let msg = body["message"].as_str().unwrap_or_default();
    assert!(msg.contains("sh.tangled.string"), "{msg}");
}

#[tokio::test]
async fn star_endpoints_accept_string_subject_form() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let resp = app
        .oneshot(list_request(
            "sh.tangled.feed.countStars",
            "at://did:plc:abalone/sh.tangled.string/k1",
            &[],
        ))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], json!(0));
}

#[tokio::test]
async fn list_after_remove_source_returns_empty_items() {
    let h = Harness::new().await;
    let subject = at("at://did:plc:abalone");
    let source = at("at://did:plc:nel/sh.tangled.feed.star/s1");
    h.add_edge(&nsid("sh.tangled.feed.star"), &subject, &source);
    h.edges.remove_source(&source);

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.feed.listStars",
            subject.as_ref(),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"], json!([]));
    assert!(body["cursor"].is_null());
}

#[tokio::test]
async fn list_pulls_hydrates_via_slingshot_when_edges_present() {
    let h = Harness::new().await;
    let target_did = did("did:plc:abalone");
    let subject = at(&format!("at://{}", target_did.as_ref()));
    let source_did = did("did:plc:nel");
    let rk = rkey("p1");
    h.add_edge(
        &nsid("sh.tangled.repo.pull"),
        &subject,
        &at(&format!(
            "at://{}/sh.tangled.repo.pull/{}",
            source_did.as_ref(),
            rk.as_ref()
        )),
    );
    h.mount(
        &source_did,
        &nsid("sh.tangled.repo.pull"),
        &rk,
        json!({
            "$type": "sh.tangled.repo.pull",
            "title": "ship it",
            "createdAt": "2026-05-01T00:00:00Z",
            "rounds": [],
            "target": {"repo": target_did.as_ref(), "branch": "main"},
        }),
    )
    .await;
    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.repo.listPulls",
            subject.as_ref(),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["value"]["title"], json!("ship it"));
    assert_eq!(
        items[0]["value"]["target"]["repo"],
        json!(target_did.as_ref())
    );
}

#[tokio::test]
async fn count_pulls_returns_distinct_authors() {
    let h = Harness::new().await;
    let subject = at("at://did:plc:abalone");
    h.add_edge(
        &nsid("sh.tangled.repo.pull"),
        &subject,
        &at("at://did:plc:nel/sh.tangled.repo.pull/p1"),
    );
    h.add_edge(
        &nsid("sh.tangled.repo.pull"),
        &subject,
        &at("at://did:plc:olaren/sh.tangled.repo.pull/p2"),
    );
    h.add_edge(
        &nsid("sh.tangled.repo.pull"),
        &subject,
        &at("at://did:plc:nel/sh.tangled.repo.pull/p3"),
    );
    let app = router(h.state.clone());
    let (_, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.repo.countPulls",
            subject.as_ref(),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(body["count"], json!(3));
    assert_eq!(body["distinctAuthors"], json!(2));
}

#[tokio::test]
async fn extractor_to_xrpc_round_trip_for_star() {
    let h = Harness::new().await;
    let subject_did = did("did:plc:abalone");
    let source_did = did("did:plc:nel");
    let rk = rkey("s1");
    let source = at(&format!(
        "at://{}/sh.tangled.feed.star/{}",
        source_did.as_ref(),
        rk.as_ref()
    ));
    let body = star_body(&subject_did);
    let parsed =
        bobbin_types::edges::Record::from_json_value(&nsid("sh.tangled.feed.star"), body.clone())
            .expect("parse star record");
    parsed
        .extract_edges(&source)
        .expect("extract")
        .into_iter()
        .for_each(|e| h.edges.add(e));
    h.mount(&source_did, &nsid("sh.tangled.feed.star"), &rk, body)
        .await;

    let app = router(h.state.clone());
    let (status, json) = json_response(
        app.oneshot(list_request(
            "sh.tangled.feed.listStars",
            &format!("at://{}", subject_did.as_ref()),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "extractor key must match handler subject, body was {json}",
    );
    let items = json["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "expected exactly one star edge");
    assert_eq!(
        items[0]["value"]["subject"]["did"],
        json!(subject_did.as_ref())
    );
}

#[tokio::test]
async fn list_issues_includes_state_comment_count_and_state_updated_at() {
    let h = Harness::new().await;
    let repo = did("did:plc:limpet");
    let subject = at(&format!("at://{}", repo.as_ref()));
    let issue_uri = at("at://did:plc:nel/sh.tangled.repo.issue/i1");
    h.add_edge(&nsid("sh.tangled.repo.issue"), &subject, &issue_uri);
    h.mount(
        &did("did:plc:nel"),
        &nsid("sh.tangled.repo.issue"),
        &rkey("i1"),
        issue_body(&repo, "hi"),
    )
    .await;
    h.add_edge(
        &nsid("sh.tangled.feed.comment"),
        &issue_uri,
        &at("at://did:plc:olaren/sh.tangled.feed.comment/c1"),
    );
    h.add_edge(
        &nsid("sh.tangled.feed.comment"),
        &issue_uri,
        &at("at://did:plc:teq/sh.tangled.feed.comment/c2"),
    );

    h.state.issue_states.upsert(
        at("at://did:plc:nel/sh.tangled.repo.issue.state/s1"),
        issue_uri.clone(),
        1_777_593_600_000_000,
        IssueStateKind::Open,
    );
    h.state.issue_states.upsert(
        at("at://did:plc:nel/sh.tangled.repo.issue.state/s2"),
        issue_uri.clone(),
        1_777_593_700_000_000,
        IssueStateKind::Closed,
    );

    let app = router(h.state.clone());
    let resp = app
        .oneshot(list_request(
            "sh.tangled.repo.listIssues",
            subject.as_ref(),
            &[],
        ))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    let item = &body["items"][0];
    assert_eq!(item["state"], json!("closed"));
    assert_eq!(item["commentCount"], json!(2));
    let updated = item["stateUpdatedAt"]
        .as_str()
        .expect("stateUpdatedAt must serialize as RFC3339 string");
    assert!(
        updated.starts_with("2026-"),
        "expected 2026 timestamp, got {updated}"
    );
}

#[tokio::test]
async fn list_issues_defaults_to_open_when_no_state_record() {
    let h = Harness::new().await;
    let repo = did("did:plc:limpet");
    let subject = at(&format!("at://{}", repo.as_ref()));
    let issue_uri = at("at://did:plc:nel/sh.tangled.repo.issue/i1");
    h.add_edge(&nsid("sh.tangled.repo.issue"), &subject, &issue_uri);
    h.mount(
        &did("did:plc:nel"),
        &nsid("sh.tangled.repo.issue"),
        &rkey("i1"),
        issue_body(&repo, "no state yet"),
    )
    .await;

    let app = router(h.state.clone());
    let (_status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.repo.listIssues",
            subject.as_ref(),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    let item = &body["items"][0];
    assert_eq!(
        item["state"],
        json!("open"),
        "absent state record defaults to open"
    );
    assert!(
        item.get("stateUpdatedAt").is_none(),
        "stateUpdatedAt must be absent without a state record",
    );
    assert_eq!(item["commentCount"], json!(0));
}

#[tokio::test]
async fn list_issues_author_filter_restricts_to_matching_did() {
    let h = Harness::new().await;
    let repo = did("did:plc:limpet");
    let subject = at(&format!("at://{}", repo.as_ref()));
    let owners = [
        ("did:plc:nel", "n1"),
        ("did:plc:nel", "n2"),
        ("did:plc:olaren", "o1"),
        ("did:plc:olaren", "o2"),
    ];
    stream::iter(owners)
        .for_each(|(d, r)| {
            let h = &h;
            let subject = subject.clone();
            let repo = repo.clone();
            async move {
                let d_did = did(d);
                let rk = rkey(r);
                h.add_edge(
                    &nsid("sh.tangled.repo.issue"),
                    &subject,
                    &at(&format!(
                        "at://{}/sh.tangled.repo.issue/{}",
                        d_did.as_ref(),
                        rk.as_ref()
                    )),
                );
                h.mount(
                    &d_did,
                    &nsid("sh.tangled.repo.issue"),
                    &rk,
                    issue_body(&repo, &format!("issue-{}", rk.as_ref())),
                )
                .await;
            }
        })
        .await;

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.repo.listIssues",
            subject.as_ref(),
            &[("author", "did:plc:nel")],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2, "two issues authored by nel");
    let all_nel = items
        .iter()
        .all(|i| i["uri"].as_str().unwrap().starts_with("at://did:plc:nel/"));
    assert!(all_nel, "every returned uri must be authored by nel");
}

#[tokio::test]
async fn list_issues_invalid_author_returns_400() {
    let h = Harness::new().await;
    let subject = "at://did:plc:limpet".to_owned();
    let app = router(h.state.clone());
    let resp = app
        .oneshot(list_request(
            "sh.tangled.repo.listIssues",
            &subject,
            &[("author", "not-a-did")],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn list_pulls_includes_merged_state_and_comment_count() {
    let h = Harness::new().await;
    let repo = did("did:plc:limpet");
    let subject = at(&format!("at://{}", repo.as_ref()));
    let pull_uri = at("at://did:plc:nel/sh.tangled.repo.pull/p1");
    h.add_edge(&nsid("sh.tangled.repo.pull"), &subject, &pull_uri);
    h.mount(
        &did("did:plc:nel"),
        &nsid("sh.tangled.repo.pull"),
        &rkey("p1"),
        pull_body(&repo, "fix bug"),
    )
    .await;
    h.add_edge(
        &nsid("sh.tangled.feed.comment"),
        &pull_uri,
        &at("at://did:plc:teq/sh.tangled.feed.comment/c1"),
    );
    h.state.pull_statuses.upsert(
        at("at://did:plc:nel/sh.tangled.repo.pull.status/s1"),
        pull_uri.clone(),
        1_777_593_600_000_000,
        PullStatusKind::Open,
    );
    h.state.pull_statuses.upsert(
        at("at://did:plc:nel/sh.tangled.repo.pull.status/s2"),
        pull_uri.clone(),
        1_777_593_800_000_000,
        PullStatusKind::Merged,
    );

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.repo.listPulls",
            subject.as_ref(),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let item = &body["items"][0];
    assert_eq!(item["state"], json!("merged"));
    assert_eq!(item["commentCount"], json!(1));
}

#[tokio::test]
async fn list_issues_state_filter_open_includes_records_without_state() {
    let h = Harness::new().await;
    let repo = did("did:plc:limpet");
    let subject = at(&format!("at://{}", repo.as_ref()));
    let issue_uri = at("at://did:plc:nel/sh.tangled.repo.issue/i1");
    h.add_edge(&nsid("sh.tangled.repo.issue"), &subject, &issue_uri);
    h.mount(
        &did("did:plc:nel"),
        &nsid("sh.tangled.repo.issue"),
        &rkey("i1"),
        issue_body(&repo, "fresh"),
    )
    .await;

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.repo.listIssues",
            subject.as_ref(),
            &[("state", "open")],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items array");
    assert_eq!(
        items.len(),
        1,
        "absent state record still matches state=open"
    );
}

#[tokio::test]
async fn list_issues_state_filter_ignores_third_party_state_source() {
    let h = Harness::new().await;
    let repo = did("did:plc:limpet");
    let subject = at(&format!("at://{}", repo.as_ref()));
    let issue_uri = at("at://did:plc:nel/sh.tangled.repo.issue/i1");
    h.add_edge(&nsid("sh.tangled.repo.issue"), &subject, &issue_uri);
    h.mount(
        &did("did:plc:nel"),
        &nsid("sh.tangled.repo.issue"),
        &rkey("i1"),
        issue_body(&repo, "open issue"),
    )
    .await;
    h.state.issue_states.upsert(
        at("at://did:plc:nautilus/sh.tangled.repo.issue.state/spoof"),
        issue_uri.clone(),
        1_777_593_800_000_000,
        IssueStateKind::Closed,
    );

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.repo.listIssues",
            subject.as_ref(),
            &[("state", "open")],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items array");
    assert_eq!(
        items.len(),
        1,
        "third-party Closed record must not flip filter result for state=open",
    );
    assert_eq!(items[0]["state"], json!("open"));
    assert!(
        items[0].get("stateUpdatedAt").is_none(),
        "third-party state source must not surface stateUpdatedAt",
    );
}

#[tokio::test]
async fn list_pulls_status_filter_ignores_third_party_status_source() {
    let h = Harness::new().await;
    let repo = did("did:plc:limpet");
    let subject = at(&format!("at://{}", repo.as_ref()));
    let pull_uri = at("at://did:plc:nel/sh.tangled.repo.pull/p1");
    h.add_edge(&nsid("sh.tangled.repo.pull"), &subject, &pull_uri);
    h.mount(
        &did("did:plc:nel"),
        &nsid("sh.tangled.repo.pull"),
        &rkey("p1"),
        pull_body(&repo, "wip"),
    )
    .await;
    h.state.pull_statuses.upsert(
        at("at://did:plc:nautilus/sh.tangled.repo.pull.status/spoof"),
        pull_uri.clone(),
        1_777_593_800_000_000,
        PullStatusKind::Merged,
    );

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.repo.listPulls",
            subject.as_ref(),
            &[("status", "merged")],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items array");
    assert_eq!(
        items.len(),
        0,
        "third-party Merged record must not satisfy status=merged"
    );
}

#[tokio::test]
async fn list_issues_state_filter_accepts_repo_owner_state_source() {
    let h = Harness::new().await;
    let repo_owner = did("did:plc:limpet");
    let subject = at(&format!("at://{}", repo_owner.as_ref()));
    let issue_uri = at("at://did:plc:nel/sh.tangled.repo.issue/i1");
    h.add_edge(&nsid("sh.tangled.repo.issue"), &subject, &issue_uri);
    h.mount(
        &did("did:plc:nel"),
        &nsid("sh.tangled.repo.issue"),
        &rkey("i1"),
        issue_body(&repo_owner, "owner closed"),
    )
    .await;
    h.state.issue_states.upsert(
        at("at://did:plc:limpet/sh.tangled.repo.issue.state/legit"),
        issue_uri.clone(),
        1_777_593_800_000_000,
        IssueStateKind::Closed,
    );

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.repo.listIssues",
            subject.as_ref(),
            &[("state", "closed")],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items array");
    assert_eq!(
        items.len(),
        1,
        "repo-owner state record must satisfy state=closed"
    );
    assert_eq!(items[0]["state"], json!("closed"));
}

#[tokio::test]
async fn list_issues_order_asc_returns_oldest_first() {
    let h = Harness::new().await;
    let repo = did("did:plc:limpet");
    let subject = at(&format!("at://{}", repo.as_ref()));
    let rkeys = ["a", "b", "c"];
    stream::iter(rkeys)
        .for_each(|r| {
            let h = &h;
            let subject = subject.clone();
            let repo = repo.clone();
            async move {
                let rk = rkey(r);
                let issue_uri = at(&format!(
                    "at://did:plc:nel/sh.tangled.repo.issue/{}",
                    rk.as_ref()
                ));
                h.add_edge(&nsid("sh.tangled.repo.issue"), &subject, &issue_uri);
                h.mount(
                    &did("did:plc:nel"),
                    &nsid("sh.tangled.repo.issue"),
                    &rk,
                    issue_body(&repo, &format!("issue-{}", rk.as_ref())),
                )
                .await;
            }
        })
        .await;

    let app = router(h.state.clone());
    let (_, asc) = json_response(
        app.clone()
            .oneshot(list_request(
                "sh.tangled.repo.listIssues",
                subject.as_ref(),
                &[("order", "asc")],
            ))
            .await
            .unwrap(),
    )
    .await;
    let (_, desc) = json_response(
        app.oneshot(list_request(
            "sh.tangled.repo.listIssues",
            subject.as_ref(),
            &[("order", "desc")],
        ))
        .await
        .unwrap(),
    )
    .await;
    let asc_uris: Vec<_> = asc["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["uri"].as_str().unwrap().to_owned())
        .collect();
    let desc_uris: Vec<_> = desc["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["uri"].as_str().unwrap().to_owned())
        .collect();
    let mut reversed = asc_uris.clone();
    reversed.reverse();
    assert_eq!(asc_uris.len(), 3);
    assert_eq!(desc_uris, reversed, "desc must be exact reverse of asc");
}

#[tokio::test]
async fn list_issues_by_state_filter_narrows_results() {
    let h = Harness::new().await;
    let author = did("did:plc:nel");
    let repo = did("did:plc:limpet");
    let open_uri = at("at://did:plc:nel/sh.tangled.repo.issue/open1");
    let closed_uri = at("at://did:plc:nel/sh.tangled.repo.issue/closed1");
    let author_subject = at(&format!("at://{}", author.as_ref()));
    h.edges.add(Edge {
        kind: nsid("sh.tangled.repo.issue.by"),
        subject: SubjectRef::Did(author.clone()),
        source: open_uri.clone(),
        sort_micros: next_sort_micros(),
    });
    h.edges.add(Edge {
        kind: nsid("sh.tangled.repo.issue.by"),
        subject: SubjectRef::Did(author.clone()),
        source: closed_uri.clone(),
        sort_micros: next_sort_micros(),
    });
    h.mount(
        &author,
        &nsid("sh.tangled.repo.issue"),
        &rkey("open1"),
        issue_body(&repo, "still open"),
    )
    .await;
    h.mount(
        &author,
        &nsid("sh.tangled.repo.issue"),
        &rkey("closed1"),
        issue_body(&repo, "shut"),
    )
    .await;
    h.state.issue_states.upsert(
        at("at://did:plc:nel/sh.tangled.repo.issue.state/s1"),
        closed_uri.clone(),
        1_777_593_800_000_000,
        IssueStateKind::Closed,
    );

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.repo.listIssuesBy",
            author_subject.as_ref(),
            &[("state", "closed")],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items array");
    assert_eq!(
        items.len(),
        1,
        "only the closed issue survives state=closed"
    );
    assert_eq!(items[0]["uri"], json!(closed_uri.as_ref()));
}
