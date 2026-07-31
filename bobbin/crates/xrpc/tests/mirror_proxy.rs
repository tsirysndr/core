use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use bobbin_edge_index::{CoverageWatch, EdgeStore, StateIndex};
use bobbin_knot_proxy::{KnotHttpConfig, KnotProxy, KnotProxyConfig, MirrorProxy};
use bobbin_record_lru::{CacheCapacity, LruRecordStore};
use bobbin_resolver::RepoIdResolver;
use bobbin_runtime::{RuntimeHasher, SystemClock};
use bobbin_search::{DEFAULT_WRITER_HEAP_BYTES, SearchIndex, SearchReader};
use bobbin_slingshot_client::SlingshotClient;
use bobbin_xrpc::{AppState, router};
use http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;
use url::Url;
use url::form_urlencoded::byte_serialize;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CID: &str = "bafyreieqygohnz2zqyvtvktbjpvhutphobcmbsnt4q5lc36ri7vpcmoz4i";
const SOCKET: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4321);
const OWNER: &str = "did:plc:nel";
const RKEY: &str = "periwinkle";
const REPO_DID: &str = "did:plc:periwinkle";
const REPO_URI: &str = "at://did:plc:nel/sh.tangled.repo/periwinkle";

const FROM_MIRROR: &str = r#"{"served_by":"mirror"}"#;
const FROM_KNOT: &str = r#"{"served_by":"knot"}"#;

enum Mirror {
    Off,
    Live,
    Unreachable,
}

fn closed_port() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

fn enc(s: &str) -> String {
    byte_serialize(s.as_bytes()).collect()
}

fn ok_from_mirror() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(FROM_MIRROR, "application/json")
}

async fn paths(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_owned())
        .collect()
}

struct Harness {
    _slingshot: MockServer,
    knot: MockServer,
    mirror: MockServer,
    state: AppState,
}

impl Harness {
    async fn new(setting: Mirror, repo_did: Option<&str>) -> Self {
        let slingshot = MockServer::start().await;
        let knot = MockServer::start().await;
        let mirror = MockServer::start().await;
        let clock = Arc::new(SystemClock::new());
        let mirror_proxy = match setting {
            Mirror::Off => None,
            Mirror::Live => Some(mirror.uri()),
            Mirror::Unreachable => Some(closed_port()),
        }
        .map(|url| {
            Arc::new(
                MirrorProxy::new(
                    &Url::parse(&url).unwrap(),
                    clock.clone(),
                    RuntimeHasher::default(),
                )
                .unwrap(),
            )
        });
        let state = AppState::new(
            Arc::new(LruRecordStore::new(CacheCapacity::from_bytes(64 * 1024))),
            SlingshotClient::with_default_http(Url::parse(&slingshot.uri()).unwrap()).unwrap(),
            Arc::new(EdgeStore::new(RuntimeHasher::default())),
            Arc::new(StateIndex::new(RuntimeHasher::default())),
            Arc::new(StateIndex::new(RuntimeHasher::default())),
            Arc::new(CoverageWatch::new()),
            Arc::new(
                KnotProxy::new(
                    KnotProxyConfig {
                        allow_private_hosts: true,
                        require_https: false,
                        ..KnotProxyConfig::default()
                    },
                    KnotHttpConfig {
                        connect_timeout: Duration::from_millis(500),
                        read_timeout: Duration::from_secs(2),
                    },
                    clock.clone(),
                    RuntimeHasher::default(),
                )
                .unwrap(),
            ),
            Arc::new(SearchIndex::new(DEFAULT_WRITER_HEAP_BYTES, clock).unwrap())
                as Arc<dyn SearchReader>,
            Arc::new(RepoIdResolver::detached(RuntimeHasher::default())),
        )
        .with_mirror(mirror_proxy);

        let mut record = json!({
            "$type": "sh.tangled.repo",
            "createdAt": "2026-05-01T00:00:00Z",
            "knot": knot.uri(),
            "name": "periwinkle",
        });
        if let Some(d) = repo_did {
            record["repoDid"] = json!(d);
        }
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.repo.getRecord"))
            .and(query_param("repo", OWNER))
            .and(query_param("collection", "sh.tangled.repo"))
            .and(query_param("rkey", RKEY))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "uri": REPO_URI,
                "cid": CID,
                "value": record,
            })))
            .mount(&slingshot)
            .await;

        Self {
            _slingshot: slingshot,
            knot,
            mirror,
            state,
        }
    }

    async fn with_mirror() -> Self {
        Self::new(Mirror::Live, Some(REPO_DID)).await
    }

    async fn mount_mirror(&self, nsid: &str, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(format!("/xrpc/{nsid}")))
            .respond_with(response)
            .mount(&self.mirror)
            .await;
    }

    async fn mount_knot(&self, nsid: &str) {
        Mock::given(method("GET"))
            .and(path(format!("/xrpc/{nsid}")))
            .respond_with(ResponseTemplate::new(200).set_body_raw(FROM_KNOT, "application/json"))
            .mount(&self.knot)
            .await;
    }

    async fn served_by(&self, nsid: &str, query: &str) -> String {
        self.served_by_with(nsid, query, &[]).await
    }

    async fn served_by_with(&self, nsid: &str, query: &str, headers: &[(&str, &str)]) -> String {
        let target = format!("/xrpc/{nsid}?repo={}&{query}", enc(REPO_URI));
        let request = headers.iter().fold(
            Request::builder()
                .uri(target)
                .extension(ConnectInfo(SOCKET)),
            |builder, (name, value)| builder.header(*name, *value),
        );
        let resp = router(self.state.clone())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .expect("router infallible");
        assert_eq!(resp.status(), StatusCode::OK, "{nsid}?{query}");
        let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }
}

#[tokio::test]
async fn tree_reads_the_mirror_keyed_on_the_repo_did() {
    let h = Harness::with_mirror().await;
    Mock::given(method("GET"))
        .and(path("/xrpc/sh.tangled.git.temp.getTree"))
        .and(query_param("repo", REPO_DID))
        .and(query_param("ref", "main"))
        .respond_with(ok_from_mirror())
        .mount(&h.mirror)
        .await;
    h.mount_knot("sh.tangled.repo.tree").await;

    assert_eq!(
        h.served_by("sh.tangled.repo.tree", "ref=main").await,
        FROM_MIRROR,
        "the mirror answers a read keyed on the repo did",
    );
    assert!(
        paths(&h.knot).await.is_empty(),
        "the knot must stay untouched"
    );
}

#[tokio::test]
async fn every_shape_compatible_request_reads_the_mirror() {
    #[rustfmt::skip]
    let routed = [
        ("sh.tangled.repo.branches", "sh.tangled.git.temp.listBranches", ""),
        ("sh.tangled.repo.log", "sh.tangled.git.temp.listCommits", "ref=main"),
        ("sh.tangled.repo.log", "sh.tangled.git.temp.listCommits", "ref=main&path="),
        ("sh.tangled.repo.tag", "sh.tangled.git.temp.getTag", "tag=v1"),
        ("sh.tangled.repo.tags", "sh.tangled.git.temp.listTags", ""),
        ("sh.tangled.repo.tree", "sh.tangled.git.temp.getTree", "ref=main"),
        ("sh.tangled.repo.tree", "sh.tangled.git.temp.getTree", "ref=main&path=crates"),
    ];
    for (knot_nsid, mirror_nsid, query) in routed {
        let h = Harness::with_mirror().await;
        h.mount_mirror(mirror_nsid, ok_from_mirror()).await;
        h.mount_knot(knot_nsid).await;
        assert_eq!(
            h.served_by(knot_nsid, query).await,
            FROM_MIRROR,
            "{knot_nsid}?{query} must read the mirror",
        );
        assert_eq!(paths(&h.mirror).await, vec![format!("/xrpc/{mirror_nsid}")]);
    }
}

#[tokio::test]
async fn every_request_the_mirror_answers_in_another_shape_reads_the_knot() {
    #[rustfmt::skip]
    let refused = [
        ("sh.tangled.repo.blob", "ref=main&path=x", "the mirror serves content types the knot answers with 403"),
        ("sh.tangled.repo.blob", "ref=main&path=x&raw=true", "raw doesn't exempt the blob"),
        ("sh.tangled.repo.archive", "ref=main", "a resume would splice knot bytes onto a mirror tarball"),
        ("sh.tangled.repo.log", "ref=main&path=crates/xrpc", "the mirror ignores path"),
        ("sh.tangled.repo.branch", "name=main", "the mirror answers branch in another shape"),
        ("sh.tangled.repo.languages", "ref=main", "the mirror answers languages in another shape"),
        ("sh.tangled.repo.compare", "", "outside the routing table"),
        ("sh.tangled.repo.describeRepo", "", "outside the routing table"),
        ("sh.tangled.repo.diff", "", "outside the routing table"),
        ("sh.tangled.repo.getDefaultBranch", "", "outside the routing table"),
        ("sh.tangled.repo.listSecrets", "", "outside the routing table"),
    ];
    for (nsid, query, why) in refused {
        let h = Harness::with_mirror().await;
        h.mount_knot(nsid).await;
        assert_eq!(h.served_by(nsid, query).await, FROM_KNOT, "{nsid}: {why}");
        assert!(paths(&h.mirror).await.is_empty(), "{nsid}: {why}");
    }
}

#[tokio::test]
async fn every_mirror_refusal_reads_the_knot() {
    for status in [400, 403, 404, 503] {
        let h = Harness::with_mirror().await;
        h.mount_mirror(
            "sh.tangled.git.temp.listBranches",
            ResponseTemplate::new(status).set_body_json(json!({"error": "BadRequest"})),
        )
        .await;
        h.mount_knot("sh.tangled.repo.branches").await;
        assert_eq!(
            h.served_by("sh.tangled.repo.branches", "limit=500").await,
            FROM_KNOT,
            "the knot must answer after a mirror {status}",
        );
    }

    let h = Harness::new(Mirror::Unreachable, Some(REPO_DID)).await;
    h.mount_knot("sh.tangled.repo.branches").await;
    assert_eq!(
        h.served_by("sh.tangled.repo.branches", "").await,
        FROM_KNOT,
        "the knot must answer when the mirror is unreachable",
    );
}

#[tokio::test]
async fn a_ranged_or_conditional_request_skips_the_mirror() {
    for (header, value) in [
        ("range", "bytes=0-99"),
        ("if-range", "bytes=0-99"),
        ("if-none-match", "\"cafe\""),
        ("if-modified-since", "Wed, 01 Jul 2026 00:00:00 GMT"),
    ] {
        let h = Harness::with_mirror().await;
        h.mount_mirror("sh.tangled.git.temp.getTree", ok_from_mirror())
            .await;
        h.mount_knot("sh.tangled.repo.tree").await;

        assert_eq!(
            h.served_by_with("sh.tangled.repo.tree", "ref=main", &[(header, value)])
                .await,
            FROM_KNOT,
            "only the knot can answer a {header} it issued",
        );
        assert!(paths(&h.mirror).await.is_empty(), "{header}");
    }
}

#[tokio::test]
async fn a_request_reads_the_knot_when_bobbin_wont_ask_the_mirror() {
    #[rustfmt::skip]
    let unasked = [
        (Mirror::Live, None, "the mirror keys on a repoDid this record doesn't have"),
        (Mirror::Off, Some(REPO_DID), "an unset mirror.url leaves every call on the knot"),
    ];
    for (setting, repo_did, why) in unasked {
        let h = Harness::new(setting, repo_did).await;
        h.mount_mirror("sh.tangled.git.temp.getTree", ok_from_mirror())
            .await;
        h.mount_knot("sh.tangled.repo.tree").await;

        assert_eq!(
            h.served_by("sh.tangled.repo.tree", "ref=main").await,
            FROM_KNOT,
            "{why}",
        );
        assert!(paths(&h.mirror).await.is_empty(), "{why}");
    }
}

#[tokio::test]
async fn the_knot_keyed_endpoints_never_read_the_mirror() {
    let h = Harness::with_mirror().await;
    Mock::given(method("GET"))
        .and(path("/xrpc/sh.tangled.knot.version"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(FROM_KNOT, "application/json"))
        .mount(&h.knot)
        .await;

    let target = format!("/xrpc/sh.tangled.knot.version?knot={}", enc(&h.knot.uri()));
    let resp = router(h.state.clone())
        .oneshot(
            Request::builder()
                .uri(target)
                .extension(ConnectInfo(SOCKET))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(paths(&h.mirror).await.is_empty());
}
