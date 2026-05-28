use std::sync::Arc;

use axum::body::{Body, to_bytes};
use bobbin_edge_index::{CoverageWatch, EdgeStore, StateIndex};
use bobbin_knot_proxy::{KnotHttpConfig, KnotProxy, KnotProxyConfig};
use bobbin_record_lru::{CacheCapacity, LruRecordStore};
use bobbin_resolver::RepoIdResolver;
use bobbin_runtime::{RuntimeHasher, SystemClock};
use bobbin_search::{DEFAULT_WRITER_HEAP_BYTES, SearchIndex, SearchReader};
use bobbin_slingshot_client::SlingshotClient;
use bobbin_types::edges::Edge;
use bobbin_types::ids::SubjectRef;
use bobbin_xrpc::{AppState, router};
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
const TAG_BYTES: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAA=";
const ARTIFACT_LINK: &str = "bafkreigh2akiscaildc7gnvtklbsfhdgwz72eolmpckbqr5ej26byp3uli";

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
    state: AppState,
}

impl Harness {
    async fn new() -> Self {
        let server = MockServer::start().await;
        let edges = Arc::new(EdgeStore::new(RuntimeHasher::default()));
        let coverage = Arc::new(CoverageWatch::new());
        let state = AppState::new(
            Arc::new(LruRecordStore::new(CacheCapacity::from_bytes(64 * 1024))),
            SlingshotClient::with_default_http(Url::parse(&server.uri()).unwrap()).unwrap(),
            edges.clone(),
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
        Self {
            server,
            edges,
            state,
        }
    }

    fn add_edge(
        &self,
        kind: &Nsid<DefaultStr>,
        subject: &AtUri<DefaultStr>,
        source: &AtUri<DefaultStr>,
    ) {
        static EDGE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        self.edges.add(Edge {
            kind: kind.clone(),
            subject: subj(subject.as_ref()),
            source: source.clone(),
            sort_micros: EDGE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
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

fn label_definition_body(name: &str) -> Value {
    json!({
        "$type": "sh.tangled.label.definition",
        "createdAt": "2026-05-01T00:00:00Z",
        "name": name,
        "scope": ["sh.tangled.repo.issue"],
        "valueType": {"type": "boolean", "format": "any"}
    })
}

fn label_op_body(subject: &AtUri<DefaultStr>, def_uri: &AtUri<DefaultStr>, value: &str) -> Value {
    json!({
        "$type": "sh.tangled.label.op",
        "performedAt": "2026-05-01T00:00:00Z",
        "subject": subject.as_ref(),
        "add": [{"key": def_uri.as_ref(), "value": value}],
        "delete": []
    })
}

fn pipeline_body(repo_did: &Did<DefaultStr>) -> Value {
    json!({
        "$type": "sh.tangled.pipeline",
        "workflows": [],
        "triggerMetadata": {
            "kind": "manual",
            "repo": {
                "did": "did:plc:teq",
                "repoDid": repo_did.as_ref(),
                "knot": "nel.pet",
                "defaultBranch": "main"
            }
        }
    })
}

fn pipeline_body_owner_only(owner_did: &Did<DefaultStr>) -> Value {
    json!({
        "$type": "sh.tangled.pipeline",
        "workflows": [],
        "triggerMetadata": {
            "kind": "manual",
            "repo": {
                "did": owner_did.as_ref(),
                "knot": "nel.pet",
                "defaultBranch": "main"
            }
        }
    })
}

fn pipeline_status_body(pipeline_uri: &AtUri<DefaultStr>) -> Value {
    json!({
        "$type": "sh.tangled.pipeline.status",
        "createdAt": "2026-05-01T00:00:00Z",
        "pipeline": pipeline_uri.as_ref(),
        "workflow": pipeline_uri.as_ref(),
        "status": "success"
    })
}

fn artifact_body(repo_did: &Did<DefaultStr>, name: &str) -> Value {
    json!({
        "$type": "sh.tangled.repo.artifact",
        "createdAt": "2026-05-01T00:00:00Z",
        "name": name,
        "repoDid": repo_did.as_ref(),
        "tag": {"$bytes": TAG_BYTES},
        "artifact": {
            "$type": "blob",
            "ref": {"$link": ARTIFACT_LINK},
            "mimeType": "application/octet-stream",
            "size": 12
        }
    })
}

fn knot_member_body(subject_did: &Did<DefaultStr>) -> Value {
    json!({
        "$type": "sh.tangled.knot.member",
        "createdAt": "2026-05-01T00:00:00Z",
        "subject": subject_did.as_ref(),
        "domain": "oyster.cafe"
    })
}

fn spindle_member_body(subject_did: &Did<DefaultStr>) -> Value {
    json!({
        "$type": "sh.tangled.spindle.member",
        "createdAt": "2026-05-01T00:00:00Z",
        "subject": subject_did.as_ref(),
        "instance": "spin.nel.pet"
    })
}

fn string_body(filename: &str, contents: &str) -> Value {
    json!({
        "$type": "sh.tangled.string",
        "createdAt": "2026-05-01T00:00:00Z",
        "filename": filename,
        "description": "test fixture",
        "contents": contents
    })
}

#[tokio::test]
async fn list_label_definitions_keys_on_owner_did() {
    let h = Harness::new().await;
    let owner = did("did:plc:abalone");
    let rk = rkey("bug");
    let source = at(&format!(
        "at://{}/sh.tangled.label.definition/{}",
        owner.as_ref(),
        rk.as_ref()
    ));
    h.add_edge(
        &nsid("sh.tangled.label.definition"),
        &at(&format!("at://{}", owner.as_ref())),
        &source,
    );
    h.mount(
        &owner,
        &nsid("sh.tangled.label.definition"),
        &rk,
        label_definition_body("bug"),
    )
    .await;

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.label.listDefinitions",
            &format!("at://{}", owner.as_ref()),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["value"]["name"], json!("bug"));
    assert_eq!(
        items[0]["value"]["scope"][0],
        json!("sh.tangled.repo.issue")
    );
}

#[tokio::test]
async fn count_label_definitions_dedupes_per_author() {
    let h = Harness::new().await;
    let owner = did("did:plc:abalone");
    let subject = at(&format!("at://{}", owner.as_ref()));
    h.add_edge(
        &nsid("sh.tangled.label.definition"),
        &subject,
        &at(&format!(
            "at://{}/sh.tangled.label.definition/bug",
            owner.as_ref()
        )),
    );
    h.add_edge(
        &nsid("sh.tangled.label.definition"),
        &subject,
        &at(&format!(
            "at://{}/sh.tangled.label.definition/wontfix",
            owner.as_ref()
        )),
    );

    let app = router(h.state.clone());
    let (_, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.label.countDefinitions",
            subject.as_ref(),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(body["count"], json!(2));
    assert_eq!(body["distinctAuthors"], json!(1));
}

#[tokio::test]
async fn list_label_ops_accepts_issue_subject() {
    let h = Harness::new().await;
    let issue_uri = at("at://did:plc:abalone/sh.tangled.repo.issue/i1");
    let author = did("did:plc:nel");
    let rk = rkey("op1");
    let def_uri = at("at://did:plc:abalone/sh.tangled.label.definition/bug");
    h.add_edge(
        &nsid("sh.tangled.label.op"),
        &issue_uri,
        &at(&format!(
            "at://{}/sh.tangled.label.op/{}",
            author.as_ref(),
            rk.as_ref()
        )),
    );
    h.mount(
        &author,
        &nsid("sh.tangled.label.op"),
        &rk,
        label_op_body(&issue_uri, &def_uri, "true"),
    )
    .await;

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.label.listOps",
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
    assert_eq!(items[0]["value"]["subject"], json!(issue_uri.as_ref()));
    assert_eq!(items[0]["value"]["add"][0]["key"], json!(def_uri.as_ref()));
}

#[tokio::test]
async fn list_label_ops_pull_subject_round_trip() {
    let h = Harness::new().await;
    let pull_uri = at("at://did:plc:abalone/sh.tangled.repo.pull/p1");
    let author = did("did:plc:bailey");
    let rk = rkey("op1");
    let def_uri = at("at://did:plc:abalone/sh.tangled.label.definition/wontfix");
    let source = at(&format!(
        "at://{}/sh.tangled.label.op/{}",
        author.as_ref(),
        rk.as_ref()
    ));
    let body = label_op_body(&pull_uri, &def_uri, "true");
    let parsed =
        bobbin_types::edges::Record::from_json_value(&nsid("sh.tangled.label.op"), body.clone())
            .expect("parse label.op record");
    parsed
        .extract_edges(&source)
        .expect("extract")
        .into_iter()
        .for_each(|e| h.edges.add(e));
    h.mount(&author, &nsid("sh.tangled.label.op"), &rk, body)
        .await;

    let app = router(h.state.clone());
    let (status, json) = json_response(
        app.oneshot(list_request(
            "sh.tangled.label.listOps",
            pull_uri.as_ref(),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = json["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["value"]["subject"], json!(pull_uri.as_ref()));
    assert_eq!(items[0]["value"]["add"][0]["key"], json!(def_uri.as_ref()));
}

#[tokio::test]
async fn list_label_ops_rejects_bare_did_subject() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.label.listOps",
            "at://did:plc:abalone",
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let msg = body["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("sh.tangled.repo.issue") && msg.contains("sh.tangled.repo.pull"),
        "message must list allowed collections, got {msg}"
    );
}

#[tokio::test]
async fn list_label_ops_rejects_unrelated_collection() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let (status, _) = json_response(
        app.oneshot(list_request(
            "sh.tangled.label.listOps",
            "at://did:plc:abalone/sh.tangled.repo/r1",
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn list_pipelines_keys_on_repo_did() {
    let h = Harness::new().await;
    let repo_did = did("did:plc:abalone");
    let subject = at(&format!("at://{}", repo_did.as_ref()));
    let spindle_did = did("did:plc:lyna");
    let rk = rkey("pl1");
    h.add_edge(
        &nsid("sh.tangled.pipeline"),
        &subject,
        &at(&format!(
            "at://{}/sh.tangled.pipeline/{}",
            spindle_did.as_ref(),
            rk.as_ref()
        )),
    );
    h.mount(
        &spindle_did,
        &nsid("sh.tangled.pipeline"),
        &rk,
        pipeline_body(&repo_did),
    )
    .await;

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.pipeline.listPipelines",
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
    assert_eq!(
        items[0]["value"]["triggerMetadata"]["repo"]["repoDid"],
        json!(repo_did.as_ref())
    );
}

#[tokio::test]
async fn count_pipelines_returns_zero_when_no_edges() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let (_, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.pipeline.countPipelines",
            "at://did:plc:abalone",
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(body["count"], json!(0));
}

#[tokio::test]
async fn list_pipeline_statuses_keys_on_pipeline_uri() {
    let h = Harness::new().await;
    let pipeline_uri = at("at://did:plc:lyna/sh.tangled.pipeline/pl1");
    let author = did("did:plc:bailey");
    let rk = rkey("s1");
    h.add_edge(
        &nsid("sh.tangled.pipeline.status"),
        &pipeline_uri,
        &at(&format!(
            "at://{}/sh.tangled.pipeline.status/{}",
            author.as_ref(),
            rk.as_ref()
        )),
    );
    h.mount(
        &author,
        &nsid("sh.tangled.pipeline.status"),
        &rk,
        pipeline_status_body(&pipeline_uri),
    )
    .await;

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.pipeline.listStatuses",
            pipeline_uri.as_ref(),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["value"]["pipeline"], json!(pipeline_uri.as_ref()));
    assert_eq!(items[0]["value"]["status"], json!("success"));
}

#[tokio::test]
async fn pipeline_status_endpoint_rejects_bare_did_subject() {
    let h = Harness::new().await;
    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.pipeline.listStatuses",
            "at://did:plc:lyna",
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("sh.tangled.pipeline/<rkey>"),
        "{body}"
    );
}

#[tokio::test]
async fn list_artifacts_keys_on_repo_did() {
    let h = Harness::new().await;
    let repo_did = did("did:plc:abalone");
    let subject = at(&format!("at://{}", repo_did.as_ref()));
    let owner = did("did:plc:nel");
    let rk = rkey("a1");
    h.add_edge(
        &nsid("sh.tangled.repo.artifact"),
        &subject,
        &at(&format!(
            "at://{}/sh.tangled.repo.artifact/{}",
            owner.as_ref(),
            rk.as_ref()
        )),
    );
    h.mount(
        &owner,
        &nsid("sh.tangled.repo.artifact"),
        &rk,
        artifact_body(&repo_did, "out.bin"),
    )
    .await;

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.repo.listArtifacts",
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
    assert_eq!(items[0]["value"]["name"], json!("out.bin"));
    assert_eq!(items[0]["value"]["repoDid"], json!(repo_did.as_ref()));
}

#[tokio::test]
async fn list_knot_members_keys_on_subject_did() {
    let h = Harness::new().await;
    let subject_did = did("did:plc:nel");
    let subject = at(&format!("at://{}", subject_did.as_ref()));
    let admin = did("did:plc:teq");
    let rk = rkey("m1");
    h.add_edge(
        &nsid("sh.tangled.knot.member"),
        &subject,
        &at(&format!(
            "at://{}/sh.tangled.knot.member/{}",
            admin.as_ref(),
            rk.as_ref()
        )),
    );
    h.mount(
        &admin,
        &nsid("sh.tangled.knot.member"),
        &rk,
        knot_member_body(&subject_did),
    )
    .await;

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.knot.listMembers",
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
    assert_eq!(items[0]["value"]["subject"], json!(subject_did.as_ref()));
    assert_eq!(items[0]["value"]["domain"], json!("oyster.cafe"));
}

#[tokio::test]
async fn list_spindle_members_keys_on_subject_did() {
    let h = Harness::new().await;
    let subject_did = did("did:plc:olaren");
    let subject = at(&format!("at://{}", subject_did.as_ref()));
    let admin = did("did:plc:teq");
    let rk = rkey("m1");
    h.add_edge(
        &nsid("sh.tangled.spindle.member"),
        &subject,
        &at(&format!(
            "at://{}/sh.tangled.spindle.member/{}",
            admin.as_ref(),
            rk.as_ref()
        )),
    );
    h.mount(
        &admin,
        &nsid("sh.tangled.spindle.member"),
        &rk,
        spindle_member_body(&subject_did),
    )
    .await;

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.spindle.listMembers",
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
    assert_eq!(items[0]["value"]["subject"], json!(subject_did.as_ref()));
    assert_eq!(items[0]["value"]["instance"], json!("spin.nel.pet"));
}

#[tokio::test]
async fn list_strings_keys_on_owner_did() {
    let h = Harness::new().await;
    let owner = did("did:plc:abalone");
    let subject = at(&format!("at://{}", owner.as_ref()));
    let rk = rkey("k1");
    h.add_edge(
        &nsid("sh.tangled.string"),
        &subject,
        &at(&format!(
            "at://{}/sh.tangled.string/{}",
            owner.as_ref(),
            rk.as_ref()
        )),
    );
    h.mount(
        &owner,
        &nsid("sh.tangled.string"),
        &rk,
        string_body("snippet.rs", "fn main() {}"),
    )
    .await;

    let app = router(h.state.clone());
    let (status, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.string.listStrings",
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
    assert_eq!(items[0]["value"]["filename"], json!("snippet.rs"));
    assert_eq!(items[0]["value"]["contents"], json!("fn main() {}"));
}

#[tokio::test]
async fn count_strings_dedupes_per_owner() {
    let h = Harness::new().await;
    let owner = did("did:plc:abalone");
    let subject = at(&format!("at://{}", owner.as_ref()));
    ["k1", "k2", "k3"].iter().for_each(|r| {
        h.add_edge(
            &nsid("sh.tangled.string"),
            &subject,
            &at(&format!("at://{}/sh.tangled.string/{}", owner.as_ref(), r)),
        );
    });

    let app = router(h.state.clone());
    let (_, body) = json_response(
        app.oneshot(list_request(
            "sh.tangled.string.countStrings",
            subject.as_ref(),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(body["count"], json!(3));
    assert_eq!(body["distinctAuthors"], json!(1));
}

#[tokio::test]
async fn extractor_to_xrpc_round_trip_for_pipeline() {
    let h = Harness::new().await;
    let repo_did = did("did:plc:abalone");
    let spindle_did = did("did:plc:lyna");
    let rk = rkey("pl1");
    let source = at(&format!(
        "at://{}/sh.tangled.pipeline/{}",
        spindle_did.as_ref(),
        rk.as_ref()
    ));
    let body = pipeline_body(&repo_did);
    let parsed =
        bobbin_types::edges::Record::from_json_value(&nsid("sh.tangled.pipeline"), body.clone())
            .expect("parse pipeline record");
    parsed
        .extract_edges(&source)
        .expect("extract")
        .into_iter()
        .for_each(|e| h.edges.add(e));
    h.mount(&spindle_did, &nsid("sh.tangled.pipeline"), &rk, body)
        .await;

    let app = router(h.state.clone());
    let (status, json) = json_response(
        app.oneshot(list_request(
            "sh.tangled.pipeline.listPipelines",
            &format!("at://{}", repo_did.as_ref()),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "extractor key must match handler subject, body was {json}"
    );
    let items = json["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "expected exactly one pipeline edge");
}

#[tokio::test]
async fn list_pipelines_drops_records_without_resolvable_repo_did() {
    let h = Harness::new().await;
    let owner_did = did("did:plc:nel");
    let spindle_did = did("did:plc:lyna");
    let rk = rkey("pl1");
    let source = at(&format!(
        "at://{}/sh.tangled.pipeline/{}",
        spindle_did.as_ref(),
        rk.as_ref()
    ));
    let body = pipeline_body_owner_only(&owner_did);
    let parsed =
        bobbin_types::edges::Record::from_json_value(&nsid("sh.tangled.pipeline"), body.clone())
            .expect("parse pipeline record");
    parsed
        .extract_edges(&source)
        .expect("extract")
        .into_iter()
        .for_each(|e| h.edges.add(e));
    h.mount(&spindle_did, &nsid("sh.tangled.pipeline"), &rk, body)
        .await;

    let app = router(h.state.clone());
    let (status, json) = json_response(
        app.oneshot(list_request(
            "sh.tangled.pipeline.listPipelines",
            &format!("at://{}", owner_did.as_ref()),
            &[],
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body was {json}");
    let items = json["items"].as_array().unwrap();
    assert!(
        items.is_empty(),
        "pipeline without resolvable repoDid must be dropped, got {json}"
    );
}
