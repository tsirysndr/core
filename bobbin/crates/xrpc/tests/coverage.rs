use std::sync::Arc;

use axum::body::{Body, to_bytes};
use bobbin_edge_index::{Coverage, CoverageWatch, EdgeStore, HydrantCursor, StateIndex};
use bobbin_knot_proxy::{KnotHttpConfig, KnotProxy, KnotProxyConfig};
use bobbin_record_lru::{CacheCapacity, LruRecordStore};
use bobbin_resolver::RepoIdResolver;
use bobbin_runtime::{RuntimeHasher, SystemClock};
use bobbin_search::{DEFAULT_WRITER_HEAP_BYTES, SearchIndex, SearchReader};
use bobbin_slingshot_client::SlingshotClient;
use bobbin_xrpc::{AppState, router};
use http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use url::Url;
use wiremock::MockServer;

struct Harness {
    coverage: Arc<CoverageWatch>,
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
        Self { coverage, state }
    }
}

fn coverage_request() -> Request<Body> {
    Request::builder()
        .uri("/xrpc/sh.tangled.bobbin.getCoverage")
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
async fn defaults_to_warming_at_zero() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let (status, body) = json_response(app.oneshot(coverage_request()).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ready"], json!(false));
    assert_eq!(body["eventsProcessed"], json!(0));
    assert_eq!(body["lastCursor"], json!(0));
}

#[tokio::test]
async fn reflects_warming_state() {
    let h = Harness::new().await;
    h.coverage.update(|_| Coverage::Warming {
        events_processed: 7,
        last_cursor: HydrantCursor::new(21),
    });
    let app = router(h.state.clone());
    let (status, body) = json_response(app.oneshot(coverage_request()).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ready"], json!(false));
    assert_eq!(body["eventsProcessed"], json!(7));
    assert_eq!(body["lastCursor"], json!(21));
}

#[tokio::test]
async fn reflects_ready_state() {
    let h = Harness::new().await;
    h.coverage.update(|_| Coverage::Ready {
        events_processed: 99,
        last_cursor: HydrantCursor::new(5000),
    });
    let app = router(h.state.clone());
    let (status, body) = json_response(app.oneshot(coverage_request()).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ready"], json!(true));
    assert_eq!(body["eventsProcessed"], json!(99));
    assert_eq!(body["lastCursor"], json!(5000));
}

#[tokio::test]
async fn promotion_flips_ready_field() {
    let h = Harness::new().await;
    let app = router(h.state.clone());

    h.coverage.update(|_| Coverage::Warming {
        events_processed: 1,
        last_cursor: HydrantCursor::new(5),
    });
    let (_, before) = json_response(app.clone().oneshot(coverage_request()).await.unwrap()).await;
    assert_eq!(before["ready"], json!(false));

    h.coverage.update(|_| Coverage::Ready {
        events_processed: 2,
        last_cursor: HydrantCursor::new(9),
    });
    let (_, after) = json_response(app.oneshot(coverage_request()).await.unwrap()).await;
    assert_eq!(after["ready"], json!(true));
    assert_eq!(after["lastCursor"], json!(9));
}
