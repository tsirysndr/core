use std::sync::Arc;

use axum::body::{Body, to_bytes};
use bobbin_edge_index::{Coverage, CoverageWatch, EdgeStore, HydrantCursor, StateIndex};
use bobbin_knot_proxy::{KnotHttpConfig, KnotProxy, KnotProxyConfig};
use bobbin_record_lru::{CacheCapacity, LruRecordStore};
use bobbin_resolver::RepoIdResolver;
use bobbin_runtime::{RuntimeHasher, SystemClock};
use bobbin_search::{DEFAULT_WRITER_HEAP_BYTES, SearchIndex, SearchReader};
use bobbin_slingshot_client::SlingshotClient;
use bobbin_types::search::{SearchDoc, SearchSink};
use bobbin_xrpc::{AppState, router};
use http::{Request, StatusCode};
use jacquard_common::DefaultStr;
use jacquard_common::types::nsid::Nsid;
use jacquard_common::types::recordkey::Rkey;
use jacquard_common::types::string::{AtUri, Did};
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

fn enc(s: &str) -> String {
    byte_serialize(s.as_bytes()).collect()
}

struct Harness {
    server: MockServer,
    coverage: Arc<CoverageWatch>,
    search: Arc<SearchIndex>,
    state: AppState,
}

impl Harness {
    async fn new() -> Self {
        let server = MockServer::start().await;
        let coverage = Arc::new(CoverageWatch::new());
        let search = Arc::new(
            SearchIndex::new(DEFAULT_WRITER_HEAP_BYTES, Arc::new(SystemClock::new())).unwrap(),
        );
        let state = AppState::new(
            Arc::new(LruRecordStore::new(CacheCapacity::from_bytes(64 * 1024))),
            SlingshotClient::with_default_http(Url::parse(&server.uri()).unwrap()).unwrap(),
            Arc::new(EdgeStore::new(RuntimeHasher::default())),
            Arc::new(StateIndex::new(RuntimeHasher::default())),
            Arc::new(StateIndex::new(RuntimeHasher::default())),
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
            search.clone() as Arc<dyn SearchReader>,
            Arc::new(RepoIdResolver::detached(RuntimeHasher::default())),
        );
        Self {
            server,
            coverage,
            search,
            state,
        }
    }

    async fn index_issue(
        &self,
        did: &Did<DefaultStr>,
        rkey: &Rkey<DefaultStr>,
        title: &str,
        body: &str,
    ) {
        self.index_issue_at(did, rkey, title, body, None, None)
            .await;
    }

    async fn index_issue_at(
        &self,
        did: &Did<DefaultStr>,
        rkey: &Rkey<DefaultStr>,
        title: &str,
        body: &str,
        created_at: Option<i64>,
        repo: Option<&Did<DefaultStr>>,
    ) {
        let uri = format!(
            "at://{}/sh.tangled.repo.issue/{}",
            did.as_ref(),
            rkey.as_ref()
        );
        self.search
            .upsert(SearchDoc {
                uri: at(&uri),
                nsid: nsid("sh.tangled.repo.issue"),
                title: title.to_owned(),
                body: body.to_owned(),
                author: Some(did.clone()),
                created_at,
                repo: repo.cloned(),
            })
            .await;
        self.search.flush().await;
        self.mount_issue(did, rkey, title, body).await;
    }

    async fn index_string(
        &self,
        did: &Did<DefaultStr>,
        rkey: &Rkey<DefaultStr>,
        filename: &str,
        contents: &str,
    ) {
        let uri = format!("at://{}/sh.tangled.string/{}", did.as_ref(), rkey.as_ref());
        self.search
            .upsert(SearchDoc {
                uri: at(&uri),
                nsid: nsid("sh.tangled.string"),
                title: filename.to_owned(),
                body: contents.to_owned(),
                author: None,
                created_at: None,
                repo: None,
            })
            .await;
        self.search.flush().await;
        self.mount_string(did, rkey, filename, contents).await;
    }

    async fn mount_issue(
        &self,
        did: &Did<DefaultStr>,
        rkey: &Rkey<DefaultStr>,
        title: &str,
        body: &str,
    ) {
        let uri = format!(
            "at://{}/sh.tangled.repo.issue/{}",
            did.as_ref(),
            rkey.as_ref()
        );
        let value = json!({
            "$type": "sh.tangled.repo.issue",
            "repo": "did:plc:abalone",
            "title": title,
            "body": body,
            "createdAt": "2026-05-01T00:00:00Z"
        });
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.repo.getRecord"))
            .and(query_param("repo", did.as_ref()))
            .and(query_param("collection", "sh.tangled.repo.issue"))
            .and(query_param("rkey", rkey.as_ref()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "uri": uri,
                "cid": CID,
                "value": value,
            })))
            .mount(&self.server)
            .await;
    }

    async fn mount_issue_with_raw_value(
        &self,
        did: &Did<DefaultStr>,
        rkey: &Rkey<DefaultStr>,
        raw_value: &str,
    ) {
        let uri = format!(
            "at://{}/sh.tangled.repo.issue/{}",
            did.as_ref(),
            rkey.as_ref()
        );
        let body = format!(r#"{{"uri":"{uri}","cid":"{CID}","value":{raw_value}}}"#,);
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.repo.getRecord"))
            .and(query_param("repo", did.as_ref()))
            .and(query_param("collection", "sh.tangled.repo.issue"))
            .and(query_param("rkey", rkey.as_ref()))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(body),
            )
            .mount(&self.server)
            .await;
    }

    async fn mount_string(
        &self,
        did: &Did<DefaultStr>,
        rkey: &Rkey<DefaultStr>,
        filename: &str,
        contents: &str,
    ) {
        let uri = format!("at://{}/sh.tangled.string/{}", did.as_ref(), rkey.as_ref());
        let value = json!({
            "$type": "sh.tangled.string",
            "filename": filename,
            "description": "field notes",
            "contents": contents,
            "createdAt": "2026-05-01T00:00:00Z"
        });
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.repo.getRecord"))
            .and(query_param("repo", did.as_ref()))
            .and(query_param("collection", "sh.tangled.string"))
            .and(query_param("rkey", rkey.as_ref()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "uri": uri,
                "cid": CID,
                "value": value,
            })))
            .mount(&self.server)
            .await;
    }

    fn warming(&self, events: u64, cursor: u64) {
        self.coverage.update(|_| Coverage::Warming {
            events_processed: events,
            last_cursor: HydrantCursor::new(cursor),
        });
    }

    fn promote_ready(&self, events: u64, cursor: u64) {
        self.coverage.update(|_| Coverage::Ready {
            events_processed: events,
            last_cursor: HydrantCursor::new(cursor),
        });
    }
}

fn search_request(extras: &[(&str, &str)]) -> Request<Body> {
    let qs = extras
        .iter()
        .map(|(k, v)| format!("{k}={}", enc(v)))
        .collect::<Vec<_>>()
        .join("&");
    Request::builder()
        .uri(format!("/xrpc/sh.tangled.search.query?{qs}"))
        .body(Body::empty())
        .unwrap()
}

async fn json_response(resp: axum::response::Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let parsed: Value = serde_json::from_slice(&bytes).expect("JSON body");
    (status, parsed)
}

#[tokio::test]
async fn empty_query_returns_no_hits() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let resp = app
        .oneshot(search_request(&[("q", "barnacle")]))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["hits"], json!([]));
    assert!(body["cursor"].is_null());
}

#[tokio::test]
async fn indexed_issue_hydrates_typed_value() {
    let h = Harness::new().await;
    h.index_issue(
        &did("did:plc:nel"),
        &rkey("abcabcabcabcz"),
        "barnacle pagination overflow",
        "scrolling resets when the cursor wraps",
    )
    .await;
    let app = router(h.state.clone());
    let resp = app
        .oneshot(search_request(&[("q", "barnacle")]))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    let hits = body["hits"].as_array().expect("hits array");
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0]["uri"].as_str().unwrap(),
        "at://did:plc:nel/sh.tangled.repo.issue/abcabcabcabcz",
    );
    assert_eq!(hits[0]["nsid"], json!("sh.tangled.repo.issue"));
    assert_eq!(hits[0]["cid"], CID);
    assert_eq!(
        hits[0]["value"]["$type"],
        json!("sh.tangled.repo.issue"),
        "hit value is typed via SearchableRecord serialization",
    );
    assert_eq!(
        hits[0]["value"]["title"],
        json!("barnacle pagination overflow"),
    );
    assert_eq!(hits[0]["value"]["repo"], json!("did:plc:abalone"));
    assert!(hits[0]["score"].as_f64().unwrap() > 0.0);
}

#[tokio::test]
async fn nsid_filter_narrows_to_single_collection() {
    let h = Harness::new().await;
    h.index_issue(
        &did("did:plc:nel"),
        &rkey("i1"),
        "anemone tide",
        "high water",
    )
    .await;
    h.index_string(
        &did("did:plc:teq"),
        &rkey("k1"),
        "anemone.md",
        "anemone notes",
    )
    .await;
    let app = router(h.state.clone());
    let resp = app
        .clone()
        .oneshot(search_request(&[
            ("q", "anemone"),
            ("nsid", "sh.tangled.string"),
        ]))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    let hits = body["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["nsid"], json!("sh.tangled.string"));
    assert_eq!(hits[0]["value"]["$type"], json!("sh.tangled.string"));
    assert_eq!(hits[0]["value"]["filename"], json!("anemone.md"));
}

#[tokio::test]
async fn hit_set_stable_across_coverage_promotion() {
    let h = Harness::new().await;
    h.index_issue(
        &did("did:plc:nel"),
        &rkey("i1"),
        "limpet survey",
        "tidal pools",
    )
    .await;
    let app = router(h.state.clone());
    h.warming(3, 5);
    let (_, before) = json_response(
        app.clone()
            .oneshot(search_request(&[("q", "limpet")]))
            .await
            .unwrap(),
    )
    .await;
    let before_hits = before["hits"].as_array().unwrap().clone();
    assert_eq!(before_hits.len(), 1);

    h.promote_ready(9, 11);
    let (_, after) = json_response(
        app.oneshot(search_request(&[("q", "limpet")]))
            .await
            .unwrap(),
    )
    .await;
    let after_hits = after["hits"].as_array().unwrap();
    assert_eq!(after_hits.len(), before_hits.len());
    assert_eq!(after_hits[0]["uri"], before_hits[0]["uri"]);
}

#[tokio::test]
async fn pagination_round_trips_via_cursor() {
    let h = Harness::new().await;
    let names = ["nel", "olaren", "teq", "lyna", "bailey"];
    for (i, owner) in names.iter().enumerate() {
        h.index_issue(
            &did(&format!("did:plc:{owner}")),
            &rkey(&format!("r{i}")),
            "anemone tides",
            "shell",
        )
        .await;
    }
    let app = router(h.state.clone());
    let (_, page1) = json_response(
        app.clone()
            .oneshot(search_request(&[("q", "anemone"), ("limit", "2")]))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(page1["hits"].as_array().unwrap().len(), 2);
    let cursor = page1["cursor"].as_str().expect("more pages").to_owned();

    let (_, page2) = json_response(
        app.clone()
            .oneshot(search_request(&[
                ("q", "anemone"),
                ("limit", "2"),
                ("cursor", &cursor),
            ]))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(page2["hits"].as_array().unwrap().len(), 2);
    let cursor2 = page2["cursor"].as_str().expect("more pages").to_owned();

    let (_, page3) = json_response(
        app.oneshot(search_request(&[
            ("q", "anemone"),
            ("limit", "2"),
            ("cursor", &cursor2),
        ]))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(page3["hits"].as_array().unwrap().len(), 1);
    assert!(page3["cursor"].is_null());
}

#[tokio::test]
async fn empty_q_returns_400() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let resp = app.oneshot(search_request(&[("q", "   ")])).await.unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], json!("InvalidRequest"));
}

#[tokio::test]
async fn invalid_cursor_returns_400() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let resp = app
        .oneshot(search_request(&[("q", "anything"), ("cursor", "not-hex")]))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], json!("InvalidRequest"));
}

#[tokio::test]
async fn invalid_nsid_returns_400() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let resp = app
        .oneshot(search_request(&[
            ("q", "anything"),
            ("nsid", "not a real nsid"),
        ]))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], json!("InvalidRequest"));
}

#[tokio::test]
async fn tombstoned_hit_silently_dropped_from_results() {
    let h = Harness::new().await;
    h.search
        .upsert(SearchDoc {
            uri: at("at://did:plc:nel/sh.tangled.repo.issue/i1"),
            nsid: nsid("sh.tangled.repo.issue"),
            title: "kelp".to_owned(),
            body: "ocean".to_owned(),
            author: None,
            created_at: None,
            repo: None,
        })
        .await;
    h.search.flush().await;
    h.index_issue(
        &did("did:plc:teq"),
        &rkey("i2"),
        "kelp survives",
        "still here",
    )
    .await;
    let app = router(h.state.clone());
    let resp = app.oneshot(search_request(&[("q", "kelp")])).await.unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    let hits = body["hits"].as_array().expect("hits array");
    assert_eq!(hits.len(), 1, "tombstoned hit dropped, sibling kept");
    assert_eq!(
        hits[0]["uri"].as_str().unwrap(),
        "at://did:plc:teq/sh.tangled.repo.issue/i2",
    );
}

#[tokio::test]
async fn upstream_5xx_during_hydration_drops_only_that_hit() {
    let h = Harness::new().await;
    h.index_issue(
        &did("did:plc:nel"),
        &rkey("i1"),
        "kelp survey",
        "still here",
    )
    .await;
    h.search
        .upsert(SearchDoc {
            uri: at("at://did:plc:teq/sh.tangled.repo.issue/i2"),
            nsid: nsid("sh.tangled.repo.issue"),
            title: "kelp drift".to_owned(),
            body: "upstream is flaky".to_owned(),
            author: Some(did("did:plc:teq")),
            created_at: None,
            repo: None,
        })
        .await;
    h.search.flush().await;
    Mock::given(method("GET"))
        .and(path("/xrpc/com.atproto.repo.getRecord"))
        .and(query_param("repo", "did:plc:teq"))
        .and(query_param("collection", "sh.tangled.repo.issue"))
        .and(query_param("rkey", "i2"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.server)
        .await;
    let app = router(h.state.clone());
    let resp = app.oneshot(search_request(&[("q", "kelp")])).await.unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    let hits = body["hits"].as_array().expect("hits array");
    assert_eq!(hits.len(), 1, "flaky hit dropped, healthy sibling kept");
    assert_eq!(
        hits[0]["uri"].as_str().unwrap(),
        "at://did:plc:nel/sh.tangled.repo.issue/i1",
    );
}

#[tokio::test]
async fn second_query_short_circuits_via_lru_without_re_querying_slingshot() {
    let h = Harness::new().await;
    h.search
        .upsert(SearchDoc {
            uri: at("at://did:plc:nel/sh.tangled.repo.issue/i1"),
            nsid: nsid("sh.tangled.repo.issue"),
            title: "kelp".to_owned(),
            body: "ocean".to_owned(),
            author: None,
            created_at: None,
            repo: None,
        })
        .await;
    h.search.flush().await;
    Mock::given(method("GET"))
        .and(path("/xrpc/com.atproto.repo.getRecord"))
        .and(query_param("repo", "did:plc:nel"))
        .and(query_param("collection", "sh.tangled.repo.issue"))
        .and(query_param("rkey", "i1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uri": "at://did:plc:nel/sh.tangled.repo.issue/i1",
            "cid": CID,
            "value": {
                "$type": "sh.tangled.repo.issue",
                "repo": "did:plc:abalone",
                "title": "kelp",
                "body": "ocean",
                "createdAt": "2026-05-01T00:00:00Z"
            }
        })))
        .expect(1)
        .mount(&h.server)
        .await;
    let app = router(h.state.clone());
    let first = app
        .clone()
        .oneshot(search_request(&[("q", "kelp")]))
        .await
        .unwrap();
    let _ = json_response(first).await;
    let second = app.oneshot(search_request(&[("q", "kelp")])).await.unwrap();
    let _ = json_response(second).await;
}

#[tokio::test]
async fn author_filter_narrows_to_matching_did() {
    let h = Harness::new().await;
    h.index_issue(&did("did:plc:nel"), &rkey("i1"), "kelp tide", "")
        .await;
    h.index_issue(&did("did:plc:teq"), &rkey("i2"), "kelp wave", "")
        .await;
    let app = router(h.state.clone());
    let resp = app
        .oneshot(search_request(&[("q", "kelp"), ("author", "did:plc:nel")]))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    let hits = body["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0]["uri"]
            .as_str()
            .unwrap()
            .starts_with("at://did:plc:nel/")
    );
}

#[tokio::test]
async fn since_until_window_filters_by_created_at() {
    let h = Harness::new().await;
    let early = 1_700_000_000;
    let mid = 1_750_000_000;
    let late = 1_800_000_000;
    h.index_issue_at(
        &did("did:plc:nel"),
        &rkey("i1"),
        "kelp early",
        "",
        Some(early),
        None,
    )
    .await;
    h.index_issue_at(
        &did("did:plc:nel"),
        &rkey("i2"),
        "kelp mid",
        "",
        Some(mid),
        None,
    )
    .await;
    h.index_issue_at(
        &did("did:plc:nel"),
        &rkey("i3"),
        "kelp late",
        "",
        Some(late),
        None,
    )
    .await;
    let app = router(h.state.clone());
    let resp = app
        .oneshot(search_request(&[
            ("q", "kelp"),
            ("since", "2025-01-01T00:00:00Z"),
            ("until", "2027-01-01T00:00:00Z"),
        ]))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    let hits = body["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "only the mid record falls in [2025, 2027)");
    assert_eq!(
        hits[0]["uri"].as_str().unwrap(),
        "at://did:plc:nel/sh.tangled.repo.issue/i2"
    );
}

#[tokio::test]
async fn repo_filter_scopes_to_owning_repo() {
    let h = Harness::new().await;
    let abalone = did("did:plc:abalone");
    let limpet = did("did:plc:limpet");
    h.index_issue_at(
        &did("did:plc:nel"),
        &rkey("i1"),
        "kelp one",
        "",
        None,
        Some(&abalone),
    )
    .await;
    h.index_issue_at(
        &did("did:plc:teq"),
        &rkey("i2"),
        "kelp two",
        "",
        None,
        Some(&limpet),
    )
    .await;
    let app = router(h.state.clone());
    let resp = app
        .oneshot(search_request(&[("q", "kelp"), ("repo", abalone.as_ref())]))
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    let hits = body["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0]["uri"].as_str().unwrap(),
        "at://did:plc:nel/sh.tangled.repo.issue/i1"
    );
}

#[tokio::test]
async fn invalid_author_did_returns_400() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let resp = app
        .oneshot(search_request(&[("q", "kelp"), ("author", "not-a-did")]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn invalid_since_returns_400() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let resp = app
        .oneshot(search_request(&[("q", "kelp"), ("since", "yesterday")]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn since_after_until_returns_400() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let resp = app
        .oneshot(search_request(&[
            ("q", "kelp"),
            ("since", "2027-01-01T00:00:00Z"),
            ("until", "2025-01-01T00:00:00Z"),
        ]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn search_recovers_hit_with_duplicate_dollar_type() {
    let h = Harness::new().await;
    let owner = did("did:plc:nel");
    let key = rkey("dupezzzzzzzzz");
    h.search
        .upsert(SearchDoc {
            uri: at(&format!(
                "at://{}/sh.tangled.repo.issue/{}",
                owner.as_ref(),
                key.as_ref()
            )),
            nsid: nsid("sh.tangled.repo.issue"),
            title: "meow regression".to_owned(),
            body: "dup type field at end of object".to_owned(),
            author: Some(owner.clone()),
            created_at: None,
            repo: None,
        })
        .await;
    h.search.flush().await;
    let raw = r#"{"$type":"sh.tangled.repo.issue","repo":"did:plc:scallop","title":"meow regression","createdAt":"2026-05-01T00:00:00Z","$type":"sh.tangled.repo.issue"}"#;
    h.mount_issue_with_raw_value(&owner, &key, raw).await;
    let app = router(h.state.clone());
    let resp = app.oneshot(search_request(&[("q", "meow")])).await.unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    let hits = body["hits"].as_array().expect("hits array");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["value"]["title"], json!("meow regression"));
    assert_eq!(hits[0]["value"]["repo"], json!("did:plc:scallop"));
}

#[tokio::test]
async fn search_drops_undecodable_hit_and_returns_others() {
    let h = Harness::new().await;
    let good_owner = did("did:plc:nel");
    let good_key = rkey("goodzzzzzzzzz");
    let bad_owner = did("did:plc:teq");
    let bad_key = rkey("badzzzzzzzzzz");
    h.index_issue(&good_owner, &good_key, "kelp survey", "good body")
        .await;
    h.search
        .upsert(SearchDoc {
            uri: at(&format!(
                "at://{}/sh.tangled.repo.issue/{}",
                bad_owner.as_ref(),
                bad_key.as_ref()
            )),
            nsid: nsid("sh.tangled.repo.issue"),
            title: "kelp drift".to_owned(),
            body: "missing required fields".to_owned(),
            author: Some(bad_owner.clone()),
            created_at: None,
            repo: None,
        })
        .await;
    h.search.flush().await;
    let unrecoverable = r#"{"$type":"sh.tangled.repo.issue"}"#;
    h.mount_issue_with_raw_value(&bad_owner, &bad_key, unrecoverable)
        .await;
    let app = router(h.state.clone());
    let resp = app.oneshot(search_request(&[("q", "kelp")])).await.unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    let hits = body["hits"].as_array().expect("hits array");
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0]["uri"].as_str().unwrap(),
        format!(
            "at://{}/sh.tangled.repo.issue/{}",
            good_owner.as_ref(),
            good_key.as_ref()
        ),
    );
}
