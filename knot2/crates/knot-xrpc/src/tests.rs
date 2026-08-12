use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::StreamExt;
use http::{HeaderMap, HeaderValue, StatusCode, header::AUTHORIZATION};
use serde_json::json;
use tempfile::TempDir;

use knot_atproto::Atproto;
use knot_git::Layout;
use knot_index::{Index, Resolved};
use knot_runtime::{
    FakeHttp, HttpRequest, HttpResponse, HttpTransport, K256Signer, ManualClock, NetworkError,
    OsEntropy, SeededEntropy, Signer, UnixMicros,
};
use knot_secrets::{MasterKey, SealedStore};
use knot_types::{
    AccountDid, AdmissionPolicy, AuthorName, Email, KnotHostname, KnotId, OriginUrl, OwnerDid,
    RepoDid, RepoName, RepoRkey,
};

use crate::XrpcState;

const KNOT_HOST: &str = "knot.nel.pet";
const ADMIN_HOST: &str = "admin.nel.pet";
const MEMBER_HOST: &str = "member.nel.pet";
const STRANGER_HOST: &str = "stranger.nel.pet";

const ADD_MEMBER: &str = "sh.tangled.knot.addMember";
const REMOVE_MEMBER: &str = "sh.tangled.knot.removeMember";
const BAN: &str = "sh.tangled.knot.ban";
const UNBAN: &str = "sh.tangled.knot.unban";
const CREATE: &str = "sh.tangled.repo.create";
const RESERVE: &str = "sh.tangled.repo.reserveKey";
const DELETE: &str = "sh.tangled.repo.delete";
const RENAME: &str = "sh.tangled.repo.rename";
const ADD_COLLAB: &str = "sh.tangled.repo.addCollaborator";
const REMOVE_COLLAB: &str = "sh.tangled.repo.removeCollaborator";
const SET_DEFAULT: &str = "sh.tangled.repo.setDefaultBranch";
const DELETE_BRANCH: &str = "sh.tangled.repo.deleteBranch";
const MERGE: &str = "sh.tangled.repo.merge";
const FORK_SYNC: &str = "sh.tangled.repo.forkSync";
const HIDDEN_REF: &str = "sh.tangled.repo.hiddenRef";

type Responder = Box<dyn Fn(&HttpRequest) -> Result<HttpResponse, NetworkError> + Send + Sync>;
type SharedState = Arc<XrpcState<FakeHttp<Responder>, ManualClock>>;

static JTI: AtomicU64 = AtomicU64::new(0);

fn knot_did() -> KnotId {
    KnotId::new(format!("did:web:{KNOT_HOST}")).unwrap()
}

fn account(host: &str) -> AccountDid {
    AccountDid::new(format!("did:web:{host}")).unwrap()
}

struct Actor {
    signer: K256Signer,
    did: AccountDid,
}

fn actor(seed: u64, host: &str) -> Actor {
    Actor {
        signer: signer(seed),
        did: account(host),
    }
}

fn signer(seed: u64) -> K256Signer {
    K256Signer::generate(&SeededEntropy::new(seed))
}

fn did_web_doc(did: &str, sec1: &[u8], pds: &str) -> Bytes {
    let multikey = knot_types::crypto::multikey(0xe7, sec1);
    Bytes::from(
        serde_json::to_vec(&json!({
            "id": did,
            "alsoKnownAs": [],
            "verificationMethod": [{
                "id": format!("{did}#atproto"),
                "type": "Multikey",
                "controller": did,
                "publicKeyMultibase": multikey
            }],
            "service": [{
                "id": "#atproto_pds",
                "type": "AtprotoPersonalDataServer",
                "serviceEndpoint": pds
            }]
        }))
        .unwrap(),
    )
}

fn repo_did_doc(did: &str, multikey: &str) -> Bytes {
    Bytes::from(
        serde_json::to_vec(&json!({
            "id": did,
            "verificationMethod": [{
                "id": format!("{did}#repo"),
                "type": "Multikey",
                "controller": did,
                "publicKeyMultibase": multikey
            }]
        }))
        .unwrap(),
    )
}

fn mint(actor: &Actor, nsid: &str) -> String {
    let jti = JTI.fetch_add(1, Ordering::Relaxed);
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"ES256K","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "iss": actor.did.as_str(),
            "aud": format!("did:web:{KNOT_HOST}"),
            "exp": 1_100,
            "iat": 999,
            "jti": format!("nonce-{jti}"),
            "lxm": nsid,
        }))
        .unwrap(),
    );
    let signing_input = format!("{header}.{payload}");
    let signature = actor.signer.sign(signing_input.as_bytes());
    format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.as_bytes())
    )
}

fn bearer(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    headers
}

fn body(value: serde_json::Value) -> Bytes {
    Bytes::from(serde_json::to_vec(&value).unwrap())
}

fn into_response(result: Result<Response, crate::XrpcError>) -> Response {
    match result {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn json_of(response: Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn call<F, Fut>(
    world: &World,
    handler: F,
    actor: &Actor,
    nsid: &str,
    value: serde_json::Value,
) -> Response
where
    F: FnOnce(State<SharedState>, HeaderMap, crate::Method, Bytes) -> Fut,
    Fut: std::future::Future<Output = Result<Response, crate::XrpcError>>,
{
    let token = mint(actor, nsid);
    into_response(
        handler(
            world.state(),
            bearer(&token),
            crate::Method::from_nsid(nsid),
            body(value),
        )
        .await,
    )
}

async fn as_member<F, Fut>(
    world: &World,
    handler: F,
    nsid: &str,
    value: serde_json::Value,
) -> Response
where
    F: FnOnce(State<SharedState>, HeaderMap, crate::Method, Bytes) -> Fut,
    Fut: std::future::Future<Output = Result<Response, crate::XrpcError>>,
{
    call(world, handler, &world.member, nsid, value).await
}

async fn as_admin<F, Fut>(
    world: &World,
    handler: F,
    nsid: &str,
    value: serde_json::Value,
) -> Response
where
    F: FnOnce(State<SharedState>, HeaderMap, crate::Method, Bytes) -> Fut,
    Fut: std::future::Future<Output = Result<Response, crate::XrpcError>>,
{
    call(world, handler, &world.admin, nsid, value).await
}

async fn as_stranger<F, Fut>(
    world: &World,
    handler: F,
    nsid: &str,
    value: serde_json::Value,
) -> Response
where
    F: FnOnce(State<SharedState>, HeaderMap, crate::Method, Bytes) -> Fut,
    Fut: std::future::Future<Output = Result<Response, crate::XrpcError>>,
{
    call(world, handler, &world.stranger, nsid, value).await
}

fn member_owner() -> OwnerDid {
    OwnerDid::new(format!("did:web:{MEMBER_HOST}")).unwrap()
}

fn resolve(world: &World, rkey: &str) -> Resolved<Option<RepoDid>> {
    world
        .state
        .index
        .resolve_repo(&member_owner(), &RepoRkey::new(rkey).unwrap())
}

fn replay(world: &World) -> Vec<std::sync::Arc<knot_events::Event>> {
    world
        .state
        .events
        .replay(
            knot_events::EventCursor::START,
            knot_events::ReplayBounds::new(
                knot_events::ReplayEvents::new(64).unwrap(),
                knot_events::ReplayBytes::new(16 << 20).unwrap(),
            ),
        )
        .events
}

fn event_count(world: &World) -> usize {
    replay(world).len()
}

fn last_event(world: &World, nsid: &str) -> std::sync::Arc<knot_events::Event> {
    replay(world)
        .into_iter()
        .rev()
        .find(|event| event.nsid == nsid)
        .unwrap_or_else(|| panic!("{nsid} event is emitted"))
}

fn git_events(world: &World) -> Vec<std::sync::Arc<knot_events::Event>> {
    replay(world)
        .into_iter()
        .filter(|event| {
            !matches!(
                event.nsid,
                "sh.tangled.knot.memberUpdate" | "sh.tangled.repo.collaboratorUpdate"
            )
        })
        .collect()
}

fn only_git_event(world: &World) -> std::sync::Arc<knot_events::Event> {
    let mut events = git_events(world);
    assert_eq!(events.len(), 1, "expected exactly one non-acl event");
    events.remove(0)
}

fn bootstrap(
    dir: &TempDir,
    rebuild: bool,
    object_format: knot_types::ObjectFormat,
) -> (Layout, Arc<Index>, PathBuf) {
    let scan_path = dir.path().join("repos");
    std::fs::create_dir_all(&scan_path).unwrap();
    let knot = knot_did();
    let layout = Layout::new(&scan_path)
        .with_object_format(object_format)
        .reserving_meta(&knot)
        .unwrap();
    layout.bootstrap_meta(&knot).unwrap();
    let meta_path = layout.meta_path(&knot).unwrap();
    let index = Arc::new(Index::new(meta_path.clone(), layout.clone()));
    if rebuild {
        index.rebuild().unwrap();
    }
    (layout, index, meta_path)
}

fn state_from(
    dir: &TempDir,
    boot: (Layout, Arc<Index>, PathBuf),
    responder: Responder,
    admission: AdmissionPolicy,
    reservations: Arc<crate::Reservations>,
    git_http: Arc<dyn HttpTransport>,
) -> SharedState {
    let (layout, index, meta_path) = boot;
    let knot = knot_did();
    let knot_url = knot_types::KnotServiceUrl::new(format!("https://{KNOT_HOST}")).unwrap();
    let atproto = Arc::new(Atproto::new(
        FakeHttp::new(responder),
        ManualClock::new(UnixMicros::new(1_000_000_000)),
        knot.clone(),
        knot_atproto::PlcDirectory::new(url::Url::parse("https://plc.directory/").unwrap())
            .unwrap(),
    ));
    let secrets = Arc::new(
        SealedStore::open(
            dir.path().join("keys.sealed"),
            &MasterKey::new([7u8; 32]).unwrap(),
            Box::new(OsEntropy),
        )
        .unwrap(),
    );
    secrets.ensure(&knot).unwrap();
    Arc::new(XrpcState {
        layout,
        index,
        atproto,
        secrets,
        entropy: Arc::new(OsEntropy),
        ci_logs: None,
        admins: BTreeSet::from([account(ADMIN_HOST)]),
        admission,
        knot_did: knot,
        knot_hostname: KnotHostname::new(KNOT_HOST).unwrap(),
        meta_path,
        knot_service_url: knot_url,
        limiter: Arc::new(crate::PreAuthLimiter::default()),
        cob_locks: Arc::new(crate::CobLocks::default()),
        reservations,
        proxy_trust: knot_types::ProxyTrust::default(),
        committer: crate::Committer {
            name: AuthorName::new("Tangled"),
            email: Email::new("noreply@tangled.sh"),
        },
        byte_limits: crate::ByteLimits::default(),
        budgets: crate::Budgets::default(),
        git_http,
        pack_limits: knot_pack::PackLimits::default(),
        service_owner: account(ADMIN_HOST),
        events: Arc::new(knot_events::EventLog::new(
            ManualClock::new(UnixMicros::new(1_000_000_000)),
            knot_events::ReplayBounds::new(
                knot_events::ReplayEvents::new(1024).unwrap(),
                knot_events::ReplayBytes::new(16 << 20).unwrap(),
            ),
        )),
        subscriber_gate: Arc::new(knot_events::SubscriberGate::new(
            knot_events::GlobalSubscriberLimit::new(16),
            knot_events::PerPeerSubscriberLimit::new(8),
        )),
        maintenance: knot_maintenance::MaintenanceHandle::disabled(),
        appview: knot_types::AppviewEndpoint::new("https://tangled.test").unwrap(),
        slots: knot_resource::Slots::testing(8),
        lfs: None,
        catalog: Arc::new(knot_messages::Catalog::defaults()),
    })
}

fn world_responder(
    pubkeys: HashMap<String, Vec<u8>>,
    repo_docs: Arc<Mutex<HashMap<String, String>>>,
    pds_records: Arc<Mutex<HashSet<String>>>,
) -> Responder {
    let doc_url = format!("https://{KNOT_HOST}");
    Box::new(move |request: &HttpRequest| {
        if request.method == http::Method::POST {
            return Ok(HttpResponse {
                status: StatusCode::OK,
                headers: http::HeaderMap::new(),
                body: Bytes::new(),
            });
        }
        if request.url.path().ends_with("com.atproto.repo.getRecord") {
            let rkey = request
                .url
                .query_pairs()
                .find(|(key, _)| key == "rkey")
                .map(|(_, value)| value.into_owned())
                .unwrap_or_default();
            let present = pds_records.lock().unwrap().contains(&rkey);
            return Ok(HttpResponse {
                status: if present {
                    StatusCode::OK
                } else {
                    StatusCode::BAD_REQUEST
                },
                headers: http::HeaderMap::new(),
                body: if present {
                    Bytes::new()
                } else {
                    Bytes::from_static(b"{\"error\":\"RecordNotFound\"}")
                },
            });
        }
        let host = request.url.host_str().unwrap_or_default();
        if let Some(multikey) = repo_docs.lock().unwrap().get(host).cloned() {
            return Ok(HttpResponse {
                status: StatusCode::OK,
                headers: http::HeaderMap::new(),
                body: repo_did_doc(&format!("did:web:{host}"), &multikey),
            });
        }
        match pubkeys.get(host) {
            Some(sec1) => Ok(HttpResponse {
                status: StatusCode::OK,
                headers: http::HeaderMap::new(),
                body: did_web_doc(&format!("did:web:{host}"), sec1, &doc_url),
            }),
            None => Ok(HttpResponse {
                status: StatusCode::NOT_FOUND,
                headers: http::HeaderMap::new(),
                body: Bytes::new(),
            }),
        }
    })
}

struct World {
    _dir: TempDir,
    layout: Layout,
    state: SharedState,
    admin: Actor,
    member: Actor,
    stranger: Actor,
    repo_docs: Arc<Mutex<HashMap<String, String>>>,
    pds_records: Arc<Mutex<HashSet<String>>>,
}

impl World {
    fn new() -> Self {
        Self::build(
            256,
            256,
            AdmissionPolicy::Closed,
            None,
            knot_types::ObjectFormat::SHA1,
        )
    }

    fn open() -> Self {
        Self::build(
            256,
            256,
            AdmissionPolicy::Open,
            None,
            knot_types::ObjectFormat::SHA1,
        )
    }

    fn with_pending_limit(limit: usize) -> Self {
        Self::build(
            limit,
            limit,
            AdmissionPolicy::Closed,
            None,
            knot_types::ObjectFormat::SHA1,
        )
    }

    fn with_limits(global: usize, per_actor: usize) -> Self {
        Self::build(
            global,
            per_actor,
            AdmissionPolicy::Closed,
            None,
            knot_types::ObjectFormat::SHA1,
        )
    }

    fn with_git_http(
        git_http: Arc<dyn HttpTransport>,
        object_format: knot_types::ObjectFormat,
    ) -> Self {
        Self::build(
            256,
            256,
            AdmissionPolicy::Closed,
            Some(git_http),
            object_format,
        )
    }

    fn build(
        global: usize,
        per_actor: usize,
        admission: AdmissionPolicy,
        git_http: Option<Arc<dyn HttpTransport>>,
        object_format: knot_types::ObjectFormat,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (layout, index, meta_path) = bootstrap(&dir, true, object_format);
        let admin = actor(1, ADMIN_HOST);
        let member = actor(2, MEMBER_HOST);
        let stranger = actor(3, STRANGER_HOST);
        let repo_docs: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
        let pds_records: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let pubkeys = HashMap::from([
            (
                ADMIN_HOST.to_string(),
                admin.signer.public_key().as_bytes().to_vec(),
            ),
            (
                MEMBER_HOST.to_string(),
                member.signer.public_key().as_bytes().to_vec(),
            ),
            (
                STRANGER_HOST.to_string(),
                stranger.signer.public_key().as_bytes().to_vec(),
            ),
        ]);
        let responder = world_responder(pubkeys, Arc::clone(&repo_docs), Arc::clone(&pds_records));
        let state = state_from(
            &dir,
            (layout.clone(), index, meta_path),
            responder,
            admission,
            Arc::new(crate::Reservations::new(
                crate::ReservationTtl::new(1_000_000),
                crate::PerActorQuota::new(per_actor),
                crate::GlobalQuota::new(global),
            )),
            git_http.unwrap_or_else(no_git_upstream),
        );
        Self {
            _dir: dir,
            layout,
            state,
            admin,
            member,
            stranger,
            repo_docs,
            pds_records,
        }
    }

    fn state(&self) -> State<SharedState> {
        State(Arc::clone(&self.state))
    }

    fn publish_repo_doc(&self, host: &str, multikey: &str) {
        self.repo_docs
            .lock()
            .unwrap()
            .insert(host.to_string(), multikey.to_string());
    }

    fn publish_pds_record(&self, rkey: &str) {
        self.pds_records.lock().unwrap().insert(rkey.to_string());
    }
}

fn build_state(responder: Responder, rebuild: bool) -> (TempDir, SharedState) {
    let dir = tempfile::tempdir().unwrap();
    let (layout, index, meta_path) = bootstrap(&dir, rebuild, knot_types::ObjectFormat::SHA1);
    let state = state_from(
        &dir,
        (layout, index, meta_path),
        responder,
        AdmissionPolicy::Closed,
        Arc::new(crate::Reservations::new(
            crate::ReservationTtl::new(1_000_000),
            crate::PerActorQuota::new(256),
            crate::GlobalQuota::new(256),
        )),
        no_git_upstream(),
    );
    (dir, state)
}

fn no_git_upstream() -> Arc<dyn HttpTransport> {
    Arc::new(FakeHttp::new(|_request: &HttpRequest| {
        Err(NetworkError::Connect(
            "no git upstream is served in this test".to_string(),
        ))
    }))
}

fn doc_responder(
    sec1: Vec<u8>,
    post_status: impl Fn() -> StatusCode + Send + Sync + 'static,
) -> Responder {
    let pds = format!("https://{KNOT_HOST}");
    Box::new(move |request: &HttpRequest| {
        let post = request.method == http::Method::POST;
        let host = request.url.host_str().unwrap_or_default();
        Ok(HttpResponse {
            status: if post { post_status() } else { StatusCode::OK },
            headers: http::HeaderMap::new(),
            body: if post {
                Bytes::new()
            } else {
                did_web_doc(&format!("did:web:{host}"), &sec1, &pds)
            },
        })
    })
}

async fn add_member_helper(world: &World) {
    assert_eq!(
        as_admin(
            world,
            crate::members::add_member,
            ADD_MEMBER,
            json!({ "subject": format!("did:web:{MEMBER_HOST}") })
        )
        .await
        .status(),
        StatusCode::OK
    );
}

async fn create_repo_helper(world: &World, name: &str) -> RepoDid {
    assert_eq!(
        as_member(
            world,
            crate::repos::create_repo,
            CREATE,
            json!({ "rkey": name, "name": name })
        )
        .await
        .status(),
        StatusCode::OK
    );
    match resolve(world, name) {
        Resolved::Ready(Some(did)) => did,
        other => panic!("repo {name} wasn't registered: {other:?}"),
    }
}

async fn create_status(world: &World, actor: &Actor, value: serde_json::Value) -> StatusCode {
    call(world, crate::repos::create_repo, actor, CREATE, value)
        .await
        .status()
}

async fn reserve_status(world: &World, actor: &Actor, did: &str) -> StatusCode {
    call(
        world,
        crate::repos::reserve_key,
        actor,
        RESERVE,
        json!({ "repoDid": did }),
    )
    .await
    .status()
}

async fn reserve_repo_key(world: &World, did_web: &str) -> String {
    let response = as_member(
        world,
        crate::repos::reserve_key,
        RESERVE,
        json!({ "repoDid": did_web }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let key = json_of(response).await["key"].as_str().unwrap().to_string();
    let host = did_web.strip_prefix("did:web:").unwrap();
    world.publish_repo_doc(host, &key);
    key
}

async fn rename_repo_as(world: &World, actor: &Actor, repo: &RepoDid, rkey: &str) -> StatusCode {
    call(
        world,
        crate::repos::rename_repo,
        actor,
        RENAME,
        json!({ "repo": repo, "rkey": rkey, "name": rkey }),
    )
    .await
    .status()
}

#[tokio::test]
async fn admission_gates_repo_creation() {
    let closed = World::new();
    let make = || json!({ "rkey": "anemone", "name": "anemone" });
    assert_eq!(
        create_status(&closed, &closed.stranger, make()).await,
        StatusCode::FORBIDDEN,
        "a closed knot denies a stranger"
    );

    let open = World::open();
    assert_eq!(
        create_status(&open, &open.stranger, make()).await,
        StatusCode::OK
    );
    assert!(
        matches!(
            open.state.index.resolve_repo(
                &OwnerDid::new(format!("did:web:{STRANGER_HOST}")).unwrap(),
                &RepoRkey::new("anemone").unwrap()
            ),
            Resolved::Ready(Some(_))
        ),
        "an open knot registers the stranger's repo without membership"
    );
}

#[tokio::test]
async fn blocklist_lifecycle() {
    let world = World::open();
    let subject = format!("did:web:{STRANGER_HOST}");
    let make = || json!({ "rkey": "anemone", "name": "anemone" });

    assert_eq!(
        as_admin(
            &world,
            crate::blocklist::ban,
            BAN,
            json!({ "subject": subject })
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert!(matches!(
        world.state.index.is_blocked(&account(STRANGER_HOST)),
        Resolved::Ready(true)
    ));
    assert_eq!(
        create_status(&world, &world.stranger, make()).await,
        StatusCode::FORBIDDEN,
        "a banned account cannot create"
    );

    assert_eq!(
        as_admin(
            &world,
            crate::blocklist::unban,
            UNBAN,
            json!({ "subject": subject })
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert!(matches!(
        world.state.index.is_blocked(&account(STRANGER_HOST)),
        Resolved::Ready(false)
    ));
    assert_eq!(
        create_status(&world, &world.stranger, make()).await,
        StatusCode::OK,
        "an unban restores creation"
    );

    assert_eq!(
        as_admin(
            &world,
            crate::blocklist::ban,
            BAN,
            json!({ "subject": format!("did:web:{ADMIN_HOST}") })
        )
        .await
        .status(),
        StatusCode::FORBIDDEN,
        "an admin cannot be banned"
    );
    assert_eq!(
        as_stranger(
            &world,
            crate::blocklist::ban,
            BAN,
            json!({ "subject": format!("did:web:{MEMBER_HOST}") })
        )
        .await
        .status(),
        StatusCode::FORBIDDEN,
        "a non-admin cannot ban"
    );
}

#[tokio::test]
async fn member_lifecycle() {
    let world = World::new();
    let subject = json!({ "subject": format!("did:web:{MEMBER_HOST}") });

    assert_eq!(
        as_admin(
            &world,
            crate::members::add_member,
            ADD_MEMBER,
            subject.clone()
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        world.state.index.is_member(&account(MEMBER_HOST)),
        Resolved::Ready(true),
        "member is effective on the very next read, with no firehose"
    );
    let events = replay(&world);
    let [added] = events.as_slice() else {
        panic!("expected exactly one event, got {}", events.len());
    };
    assert_eq!(added.nsid, "sh.tangled.knot.memberUpdate");
    assert_eq!(added.payload["op"], "add");
    assert_eq!(added.payload["subject"], account(MEMBER_HOST).to_string());

    let baseline = event_count(&world);
    assert_eq!(
        as_admin(
            &world,
            crate::members::add_member,
            ADD_MEMBER,
            subject.clone()
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        event_count(&world),
        baseline,
        "a redundant add is a no-op and emits no event"
    );

    assert_eq!(
        as_admin(
            &world,
            crate::members::remove_member,
            REMOVE_MEMBER,
            subject.clone()
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        world.state.index.is_member(&account(MEMBER_HOST)),
        Resolved::Ready(false),
        "removed member is gone on the very next read"
    );
    let removed = last_event(&world, "sh.tangled.knot.memberUpdate");
    assert_eq!(removed.payload["op"], "remove");
    assert_eq!(removed.payload["subject"], account(MEMBER_HOST).to_string());

    let baseline = event_count(&world);
    assert_eq!(
        as_admin(
            &world,
            crate::members::remove_member,
            REMOVE_MEMBER,
            subject
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        event_count(&world),
        baseline,
        "removing a non-member is a no-op and emits no event"
    );
}

#[tokio::test]
async fn add_member_auth_outcomes() {
    struct Case {
        headers: HeaderMap,
        body: serde_json::Value,
        status: StatusCode,
        why: &'static str,
    }

    let world = World::new();
    let lowercase = {
        let token = mint(&world.admin, ADD_MEMBER);
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("bearer {token}")).unwrap(),
        );
        headers
    };
    let cases = vec![
        Case {
            headers: bearer(&mint(&world.member, ADD_MEMBER)),
            body: json!({ "subject": "did:web:olaren.dev" }),
            status: StatusCode::FORBIDDEN,
            why: "a non-admin cannot add a member",
        },
        Case {
            headers: HeaderMap::new(),
            body: json!({ "subject": format!("did:web:{MEMBER_HOST}") }),
            status: StatusCode::UNAUTHORIZED,
            why: "a request without a token is unauthorized",
        },
        Case {
            headers: bearer(&mint(&world.admin, ADD_MEMBER)),
            body: json!({ "subject": "not-a-did" }),
            status: StatusCode::BAD_REQUEST,
            why: "an invalid DID is rejected at decode by the newtype Deserialize, never reaching a handler",
        },
        Case {
            headers: lowercase,
            body: json!({ "subject": "did:web:olaren.dev" }),
            status: StatusCode::OK,
            why: "the bearer scheme is matched case-insensitively per RFC 7235",
        },
    ];

    let world_ref = &world;
    futures::stream::iter(cases)
        .for_each(|case| async move {
            let status = into_response(
                crate::members::add_member(
                    world_ref.state(),
                    case.headers,
                    crate::Method::from_nsid(ADD_MEMBER),
                    body(case.body),
                )
                .await,
            )
            .status();
            assert_eq!(status, case.status, "{}", case.why);
        })
        .await;
}

#[tokio::test]
async fn a_transient_upstream_identity_failure_is_a_503_named_upstream_unavailable() {
    let responder: Responder = Box::new(|_request: &HttpRequest| {
        Ok(HttpResponse {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            headers: http::HeaderMap::new(),
            body: Bytes::new(),
        })
    });
    let (_dir, state) = build_state(responder, true);
    let admin = actor(1, ADMIN_HOST);
    let token = mint(&admin, ADD_MEMBER);
    let response = into_response(
        crate::members::add_member(
            State(state),
            bearer(&token),
            crate::Method::from_nsid(ADD_MEMBER),
            body(json!({ "subject": format!("did:web:{MEMBER_HOST}") })),
        )
        .await,
    );
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "transient issuer-doc failure is 503, distinct from a bad token's 401"
    );
    assert_eq!(
        json_of(response).await["error"],
        "UpstreamUnavailable",
        "transient upstream identity failure is named distinctly from a warming projection"
    );
}

#[tokio::test]
async fn concurrent_first_member_adds_converge_to_a_single_cob_object() {
    use knot_cob::CobStore;
    use knot_cobs::MembersCob;
    use knot_git::Repo;

    let world = World::new();
    let (left, right) = tokio::join!(
        as_admin(
            &world,
            crate::members::add_member,
            ADD_MEMBER,
            json!({ "subject": "did:web:witchcraft.systems" })
        ),
        as_admin(
            &world,
            crate::members::add_member,
            ADD_MEMBER,
            json!({ "subject": "did:web:isabelroses.com" })
        ),
    );
    assert_eq!(left.status(), StatusCode::OK);
    assert_eq!(right.status(), StatusCode::OK);

    let meta = Repo::open(world.state.meta_path.clone()).unwrap();
    assert_eq!(
        CobStore::new(&meta).list::<MembersCob>().unwrap().len(),
        1,
        "concurrent first adds serialize onto one singleton members COB, never splitting it"
    );
    assert_eq!(
        world.state.index.is_member(&account("witchcraft.systems")),
        Resolved::Ready(true)
    );
    assert_eq!(
        world.state.index.is_member(&account("isabelroses.com")),
        Resolved::Ready(true)
    );
}

#[tokio::test]
async fn re_adding_a_member_under_warming_appends_no_redundant_change() {
    use knot_cob::CobStore;
    use knot_cobs::{Grant, MembersChange, MembersCob};
    use knot_git::Repo;

    let admin = actor(1, ADMIN_HOST);
    let responder = doc_responder(admin.signer.public_key().as_bytes().to_vec(), || {
        StatusCode::OK
    });
    let (_dir, state) = build_state(responder, false);

    let now = state.now();
    let knot_signer = state.secrets.signer(&state.knot_did).unwrap();
    let subject = account(MEMBER_HOST);
    let meta = Repo::open(state.meta_path.clone()).unwrap();
    let created = CobStore::new(&meta)
        .create(
            &knot_cob::CobHome::from(&state.knot_did),
            &MembersChange::Add(Grant {
                subject: subject.clone(),
                added_by: account(ADMIN_HOST),
                created_at: now,
            }),
            &knot_signer,
            now,
        )
        .unwrap();

    let token = mint(&admin, ADD_MEMBER);
    assert_eq!(
        into_response(
            crate::members::add_member(
                State(Arc::clone(&state)),
                bearer(&token),
                crate::Method::from_nsid(ADD_MEMBER),
                body(json!({ "subject": subject.as_str() })),
            )
            .await
        )
        .status(),
        StatusCode::OK
    );

    let meta = Repo::open(state.meta_path.clone()).unwrap();
    let delta = CobStore::new(&meta)
        .changes_since::<MembersCob>(created.object, Some(created.tip))
        .unwrap();
    assert!(
        delta.changes.is_empty(),
        "re-adding an existing member appends no change, even while projection is warming"
    );
}

#[tokio::test]
async fn create_mints_a_did_plc_repo_and_refuses_a_duplicate_name() {
    let world = World::new();
    add_member_helper(&world).await;

    let repo_did = create_repo_helper(&world, "anemone").await;
    assert!(
        repo_did.as_str().starts_with("did:plc:"),
        "knot minted a did:plc identity for the repo"
    );
    assert!(
        world.layout.open(&repo_did).is_ok(),
        "bare repo exists on disk under its minted DID"
    );
    assert_eq!(
        world.state.secrets.len(),
        1,
        "only the shared knot key is sealed"
    );

    assert_eq!(
        create_status(
            &world,
            &world.member,
            json!({ "rkey": "anemone", "name": "anemone" })
        )
        .await,
        StatusCode::CONFLICT,
        "a second repo of the same name is refused, never silently overwritten"
    );
    assert_eq!(
        resolve(&world, "anemone"),
        Resolved::Ready(Some(repo_did.clone())),
        "the original repo still owns the name"
    );
    assert!(
        world.layout.open(&repo_did).is_ok(),
        "the original repo is untouched on disk"
    );
}

#[tokio::test]
async fn create_and_reserve_reject_bad_identities() {
    let world = World::new();
    add_member_helper(&world).await;

    assert_eq!(
        create_status(
            &world,
            &world.member,
            json!({ "rkey": "a", "name": "a", "repoDid": "did:plc:aaaaaaaaaaaaaaaaaaaaaaaa" })
        )
        .await,
        StatusCode::BAD_REQUEST,
        "a did:plc cannot be brought; the knot mints those itself"
    );

    assert_eq!(
        create_status(
            &world,
            &world.member,
            json!({ "rkey": "evil", "name": "evil", "repoDid": format!("did:web:{KNOT_HOST}") })
        )
        .await,
        StatusCode::BAD_REQUEST,
        "the knot's own DID as a repoDid is a client error instead of a 500"
    );
    assert_eq!(
        world.state.index.is_member(&account(MEMBER_HOST)),
        Resolved::Ready(true),
        "the meta-repo is intact"
    );

    assert_eq!(
        create_status(
            &world,
            &world.member,
            json!({ "rkey": "uni", "name": "uni", "repoDid": "did:web:uni.olaren.dev" })
        )
        .await,
        StatusCode::BAD_REQUEST,
        "a did:web create without a prior reserveKey is refused"
    );
    assert_eq!(
        resolve(&world, "uni"),
        Resolved::Ready(None),
        "nothing was registered for the unproven did:web"
    );

    assert_eq!(
        reserve_status(&world, &world.member, &format!("did:web:{KNOT_HOST}")).await,
        StatusCode::BAD_REQUEST,
        "the knot's own DID cannot have a repo key reserved against it"
    );

    let unpublished = "did:web:conch.olaren.dev";
    assert_eq!(
        reserve_status(&world, &world.member, unpublished).await,
        StatusCode::OK
    );
    let impostor = knot_types::crypto::multikey(0xe7, signer(99).public_key().as_bytes());
    world.publish_repo_doc("conch.olaren.dev", &impostor);
    assert_eq!(
        create_status(
            &world,
            &world.member,
            json!({ "rkey": "conch", "name": "conch", "repoDid": unpublished })
        )
        .await,
        StatusCode::BAD_REQUEST,
        "a did:web whose document publishes a different key fails the control proof"
    );
    assert!(
        world
            .layout
            .open(&RepoDid::new(unpublished).unwrap())
            .is_err(),
        "the repo was never created on disk"
    );

    let victim = "did:web:victim.olaren.dev";
    let reserved = reserve_repo_key(&world, victim).await;
    assert_eq!(
        reserve_repo_key(&world, victim).await,
        reserved,
        "re-reserving returns the same key so an already-published document stays valid"
    );
    assert_eq!(
        create_status(
            &world,
            &world.admin,
            json!({ "rkey": "victim", "name": "victim", "repoDid": victim })
        )
        .await,
        StatusCode::BAD_REQUEST,
        "only the account that reserved the did:web may create it"
    );
    assert!(
        world.layout.open(&RepoDid::new(victim).unwrap()).is_err(),
        "the hijack attempt created no repo on disk"
    );

    let before = world.state.secrets.len();
    assert_eq!(
        create_status(
            &world,
            &world.member,
            json!({ "rkey": "doomed", "name": "doomed", "defaultBranch": "bad..name" })
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        world.state.secrets.len(),
        before,
        "an invalid branch is rejected before any key is minted, sealed, or DID submitted"
    );
}

#[tokio::test]
async fn a_byo_did_web_repo_is_accepted_and_its_key_is_returned() {
    let world = World::new();
    add_member_helper(&world).await;
    let did = "did:web:nautilus.olaren.dev";
    let reserved_key = reserve_repo_key(&world, did).await;

    let response = as_member(
        &world,
        crate::repos::create_repo,
        CREATE,
        json!({ "rkey": "nautilus", "name": "nautilus", "repoDid": did }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let output = json_of(response).await;
    assert_eq!(output["repoDid"].as_str(), Some(did));

    let repo_did = RepoDid::new(did).unwrap();
    assert!(
        world.layout.open(&repo_did).is_ok(),
        "bring-your-own did:web repo is on disk"
    );
    let knot_public = world
        .state
        .secrets
        .public_key(&world.state.knot_did)
        .unwrap();
    let expected_key = knot_types::crypto::multikey(0xe7, knot_public.as_bytes());
    assert_eq!(
        expected_key, reserved_key,
        "reserve returns the knot key the owner publishes in their did:web document"
    );
    assert_eq!(
        output["key"].as_str(),
        Some(expected_key.as_str()),
        "create returns the knot-held key the owner published in their own did:web document"
    );

    assert_eq!(
        as_member(
            &world,
            crate::collaborators::add_collaborator,
            ADD_COLLAB,
            json!({ "repo": did, "subject": "did:web:witchcraft.systems" })
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        world
            .state
            .index
            .is_collaborator(&repo_did, &account("witchcraft.systems")),
        Resolved::Ready(true),
        "the collaborator COB signed by the knot-held repo key lands and is visible"
    );

    let squid = "did:web:squid.olaren.dev";
    let victim = RepoDid::new(squid).unwrap();
    world.layout.create(&victim).unwrap();
    reserve_repo_key(&world, squid).await;
    assert_eq!(
        create_status(
            &world,
            &world.member,
            json!({ "rkey": "anemone", "name": "anemone", "repoDid": squid })
        )
        .await,
        StatusCode::CONFLICT
    );
    assert!(
        world.layout.open(&victim).is_ok(),
        "a colliding create mustn't delete the repository already on disk"
    );
}

#[tokio::test]
async fn a_rejected_plc_submission_is_a_bad_gateway() {
    let admin = actor(1, ADMIN_HOST);
    let key = admin.signer.public_key().as_bytes().to_vec();
    let responder = doc_responder(key, || StatusCode::BAD_REQUEST);
    let (_dir, state) = build_state(responder, true);
    let token = mint(&admin, CREATE);
    let status = into_response(
        crate::repos::create_repo(
            State(Arc::clone(&state)),
            bearer(&token),
            crate::Method::from_nsid(CREATE),
            body(json!({ "rkey": "conch", "name": "conch" })),
        )
        .await,
    )
    .status();
    assert_eq!(
        status,
        StatusCode::BAD_GATEWAY,
        "non-transient PLC rejection surfaces as 502, distinct from an internal 500"
    );
    assert_eq!(
        state.secrets.len(),
        1,
        "rejected PLC submission rolls back the minted repo key, leaving only the knot's own. Unpublished did:plc orphans nothing"
    );
    assert_eq!(
        state.index.resolve_repo(
            &OwnerDid::new(format!("did:web:{ADMIN_HOST}")).unwrap(),
            &RepoRkey::new("conch").unwrap()
        ),
        Resolved::Ready(None),
        "repo isn't registered after a rejected PLC submission"
    );
}

#[tokio::test]
async fn a_rejected_plc_submission_never_touches_the_registry() {
    let admin = actor(1, ADMIN_HOST);
    let key = admin.signer.public_key().as_bytes().to_vec();
    let reject_posts = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reject = Arc::clone(&reject_posts);
    let responder = doc_responder(key, move || {
        if reject.load(Ordering::Relaxed) {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::OK
        }
    });
    let (_dir, state) = build_state(responder, true);
    let owner = OwnerDid::new(format!("did:web:{ADMIN_HOST}")).unwrap();

    let mint_create = || mint(&admin, CREATE);
    let anemone = || body(json!({ "rkey": "anemone", "name": "anemone" }));
    assert_eq!(
        into_response(
            crate::repos::create_repo(
                State(Arc::clone(&state)),
                bearer(&mint_create()),
                crate::Method::from_nsid(CREATE),
                anemone(),
            )
            .await
        )
        .status(),
        StatusCode::OK
    );
    let victim = match state
        .index
        .resolve_repo(&owner, &RepoRkey::new("anemone").unwrap())
    {
        Resolved::Ready(Some(did)) => did,
        other => panic!("victim repo wasn't registered: {other:?}"),
    };

    let token = mint(&admin, RENAME);
    assert_eq!(
        into_response(
            crate::repos::rename_repo(
                State(Arc::clone(&state)),
                bearer(&token),
                crate::Method::from_nsid(RENAME),
                body(json!({ "repo": victim.as_str(), "rkey": "barnacle", "name": "barnacle" })),
            )
            .await
        )
        .status(),
        StatusCode::OK
    );

    reject_posts.store(true, Ordering::Relaxed);
    assert_eq!(
        into_response(
            crate::repos::create_repo(
                State(Arc::clone(&state)),
                bearer(&mint_create()),
                crate::Method::from_nsid(CREATE),
                anemone(),
            )
            .await
        )
        .status(),
        StatusCode::BAD_GATEWAY
    );

    assert_eq!(
        state
            .index
            .resolve_repo(&owner, &RepoRkey::new("anemone").unwrap()),
        Resolved::Ready(Some(victim.clone())),
        "stale alias still resolves to its prior holder because the failed create never registered"
    );

    state.index.refresh_registry().unwrap();
    assert_eq!(
        state
            .index
            .resolve_repo(&owner, &RepoRkey::new("anemone").unwrap()),
        Resolved::Ready(Some(victim.clone())),
        "durable registry COB has no trace of the failed create"
    );
    assert_eq!(
        state.index.rkey_of(&victim),
        Resolved::Ready(Some(RepoRkey::new("barnacle").unwrap())),
        "victim's canonical rkey is unmoved"
    );
}

#[tokio::test]
async fn resolve_by_name_matches_the_rkey_case_sensitively() {
    let admin = actor(1, ADMIN_HOST);
    let key = admin.signer.public_key().as_bytes().to_vec();
    let responder = doc_responder(key, || StatusCode::OK);
    let (_dir, state) = build_state(responder, true);
    let owner = OwnerDid::new(format!("did:web:{ADMIN_HOST}")).unwrap();

    assert_eq!(
        into_response(
            crate::repos::create_repo(
                State(Arc::clone(&state)),
                bearer(&mint(&admin, CREATE)),
                crate::Method::from_nsid(CREATE),
                body(json!({ "rkey": "anemone", "name": "anemone" })),
            )
            .await
        )
        .status(),
        StatusCode::OK
    );

    assert!(
        crate::merge::resolve_by_name(&*state, &owner, &RepoName::new("anemone").unwrap()).is_ok(),
        "the exact rkey resolves"
    );
    assert!(
        crate::merge::resolve_by_name(&*state, &owner, &RepoName::new("Anemone").unwrap()).is_err(),
        "a differently-cased name must not resolve to a distinct rkey, atproto record keys are case-sensitive"
    );
}

#[tokio::test]
async fn reserve_key_refuses_once_the_pending_limit_is_reached() {
    let world = World::with_pending_limit(2);
    add_member_helper(&world).await;

    assert_eq!(
        reserve_status(&world, &world.member, "did:web:p0.olaren.dev").await,
        StatusCode::OK,
        "first reservation is within the limit"
    );
    assert_eq!(
        reserve_status(&world, &world.member, "did:web:p1.olaren.dev").await,
        StatusCode::OK,
        "second reservation reaches the limit"
    );
    assert_eq!(
        reserve_status(&world, &world.member, "did:web:p2.olaren.dev").await,
        StatusCode::TOO_MANY_REQUESTS,
        "member cannot grow the sealed store without bound past the pending-reservation limit"
    );
}

#[tokio::test]
async fn one_account_cannot_exhaust_the_global_reservation_budget() {
    let world = World::with_limits(256, 2);
    add_member_helper(&world).await;

    let world_ref = &world;
    futures::stream::iter(["did:web:m0.olaren.dev", "did:web:m1.olaren.dev"])
        .for_each(|did| async move {
            assert_eq!(
                reserve_status(world_ref, &world_ref.member, did).await,
                StatusCode::OK
            );
        })
        .await;
    assert_eq!(
        reserve_status(&world, &world.member, "did:web:m2.olaren.dev").await,
        StatusCode::TOO_MANY_REQUESTS,
        "member is held to its per-actor reservation budget"
    );
    assert_eq!(
        reserve_status(&world, &world.admin, "did:web:admin0.olaren.dev").await,
        StatusCode::OK,
        "a different account keeps its own budget while global capacity remains"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_reserve_key_calls_all_succeed() {
    let world = World::new();
    add_member_helper(&world).await;

    let handles: Vec<_> = (0..24)
        .map(|i| {
            let state = world.state();
            let token = mint(&world.member, RESERVE);
            let payload = body(json!({ "repoDid": format!("did:web:r{i}.olaren.dev") }));
            tokio::spawn(async move {
                into_response(
                    crate::repos::reserve_key(
                        state,
                        bearer(&token),
                        crate::Method::from_nsid(RESERVE),
                        payload,
                    )
                    .await,
                )
                .status()
            })
        })
        .collect();

    futures::future::join_all(handles)
        .await
        .into_iter()
        .for_each(|result| {
            assert_eq!(
                result.unwrap(),
                StatusCode::OK,
                "concurrent reserveKey mustn't race the in-memory reservation map"
            )
        });
}

#[tokio::test]
async fn collaborator_lifecycle() {
    let world = World::new();
    add_member_helper(&world).await;
    let repo_did = create_repo_helper(&world, "scallop").await;
    let subject = || json!({ "repo": repo_did, "subject": "did:web:witchcraft.systems" });

    assert_eq!(
        as_member(
            &world,
            crate::collaborators::add_collaborator,
            ADD_COLLAB,
            subject()
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        world
            .state
            .index
            .is_collaborator(&repo_did, &account("witchcraft.systems")),
        Resolved::Ready(true)
    );
    let added = last_event(&world, "sh.tangled.repo.collaboratorUpdate");
    assert_eq!(added.payload["op"], "add");
    assert_eq!(
        added.payload["subject"],
        account("witchcraft.systems").to_string()
    );
    assert_eq!(added.payload["repo"], repo_did.to_string());

    assert_eq!(
        as_member(
            &world,
            crate::collaborators::remove_collaborator,
            REMOVE_COLLAB,
            subject()
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        world
            .state
            .index
            .is_collaborator(&repo_did, &account("witchcraft.systems")),
        Resolved::Ready(false),
        "removed collaborator is gone on the very next read"
    );
    let removed = last_event(&world, "sh.tangled.repo.collaboratorUpdate");
    assert_eq!(removed.payload["op"], "remove");
    assert_eq!(
        removed.payload["subject"],
        account("witchcraft.systems").to_string()
    );
    assert_eq!(removed.payload["repo"], repo_did.to_string());

    let baseline = event_count(&world);
    assert_eq!(
        as_member(
            &world,
            crate::collaborators::remove_collaborator,
            REMOVE_COLLAB,
            subject()
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        event_count(&world),
        baseline,
        "removing a non-collaborator is a no-op and emits no event"
    );
}

#[tokio::test]
async fn repo_management_is_owner_or_collaborator_gated() {
    let world = World::new();
    add_member_helper(&world).await;
    let repo_did = create_repo_helper(&world, "squid").await;

    assert_eq!(
        as_admin(
            &world,
            crate::collaborators::add_collaborator,
            ADD_COLLAB,
            json!({ "repo": repo_did, "subject": "did:web:isabelroses.com" })
        )
        .await
        .status(),
        StatusCode::FORBIDDEN,
        "collaborator management is the repo owner's right instead of a knot admin's"
    );
    assert_eq!(
        as_stranger(
            &world,
            crate::branches::set_default_branch,
            SET_DEFAULT,
            json!({ "repo": repo_did, "defaultBranch": "trunk" })
        )
        .await
        .status(),
        StatusCode::FORBIDDEN,
        "a stranger cannot set the default branch"
    );
    assert_eq!(
        as_stranger(
            &world,
            crate::branches::delete_branch,
            DELETE_BRANCH,
            json!({ "repo": repo_did, "branch": "trunk" })
        )
        .await
        .status(),
        StatusCode::FORBIDDEN,
        "a stranger cannot delete a branch"
    );
    assert_eq!(
        as_stranger(
            &world,
            crate::repos::delete_repo,
            DELETE,
            json!({ "repo": repo_did })
        )
        .await
        .status(),
        StatusCode::FORBIDDEN,
        "neither owner nor a knot admin, so delete is refused"
    );

    assert_eq!(
        rename_repo_as(&world, &world.stranger, &repo_did, "stolen").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        world.state.index.rkey_of(&repo_did),
        Resolved::Ready(Some(RepoRkey::new("squid").unwrap())),
        "the canonical rkey is untouched by the rejected rename"
    );

    assert_eq!(
        as_member(
            &world,
            crate::collaborators::add_collaborator,
            ADD_COLLAB,
            json!({ "repo": repo_did, "subject": format!("did:web:{STRANGER_HOST}") })
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        rename_repo_as(&world, &world.stranger, &repo_did, "periwinkle").await,
        StatusCode::OK,
        "rename is gated by can_push, so a collaborator may rename"
    );
}

#[tokio::test]
async fn the_owner_sets_the_default_branch_by_repo_did() {
    let world = World::new();
    add_member_helper(&world).await;
    let repo_did = create_repo_helper(&world, "mussel").await;

    assert_eq!(
        as_member(
            &world,
            crate::branches::set_default_branch,
            SET_DEFAULT,
            json!({ "repo": repo_did, "defaultBranch": "trunk" })
        )
        .await
        .status(),
        StatusCode::OK
    );

    let repo = world.layout.open(&repo_did).unwrap();
    assert_eq!(
        repo.default_branch().map(|name| name.as_str().to_string()),
        Some("refs/heads/trunk".to_string()),
        "HEAD now points at the requested default branch"
    );

    let event = only_git_event(&world);
    assert_eq!(event.nsid, "sh.tangled.git.refUpdate");
    assert_eq!(event.payload["repo"], repo_did.to_string());
    assert_eq!(event.payload["ownerDid"], account(MEMBER_HOST).to_string());
    assert_eq!(
        event.payload["committerDid"],
        account(MEMBER_HOST).to_string(),
        "the actor who set the default branch is the committer on the wire"
    );
}

#[tokio::test]
async fn set_default_branch_rejections() {
    use knot_cob::CobStore;
    use knot_cobs::{CollaboratorsChange, Grant};
    use knot_git::RefUpdate;
    use knot_types::RefName;

    let world = World::new();
    add_member_helper(&world).await;
    let repo_did = create_repo_helper(&world, "mussel").await;

    let git = world.layout.open(&repo_did).unwrap();
    let now = world.state.now();
    let signer = world.state.secrets.signer(&world.state.knot_did).unwrap();
    let created = CobStore::new(&git)
        .create(
            &knot_cob::CobHome::from(&repo_did),
            &CollaboratorsChange::Add(Grant {
                subject: account("olaren.dev"),
                added_by: account("olaren.dev"),
                created_at: now,
            }),
            &signer,
            now,
        )
        .unwrap();
    git.update_ref(&RefUpdate::Create {
        name: RefName::new("refs/heads/main").unwrap(),
        new: created.tip.oid(),
    })
    .unwrap();

    assert_eq!(
        as_member(
            &world,
            crate::branches::set_default_branch,
            SET_DEFAULT,
            json!({ "repo": "did:web:squid.oyster.cafe", "defaultBranch": "trunk" })
        )
        .await
        .status(),
        StatusCode::NOT_FOUND,
        "we'll reject a repo DID that this knot doesn't host"
    );
    assert_eq!(
        as_member(
            &world,
            crate::branches::set_default_branch,
            SET_DEFAULT,
            json!({ "repo": repo_did, "defaultBranch": "ghost" })
        )
        .await
        .status(),
        StatusCode::NOT_FOUND,
        "a populated repo rejects a default pointing at a branch that doesn't exist"
    );
}

#[tokio::test]
async fn delete_branch_removes_a_branch_and_refuses_the_default() {
    use knot_cob::CobStore;
    use knot_cobs::{CollaboratorsChange, Grant};
    use knot_git::RefUpdate;
    use knot_types::RefName;

    let world = World::new();
    add_member_helper(&world).await;
    let repo_did = create_repo_helper(&world, "periwinkle").await;
    let git = world.layout.open(&repo_did).unwrap();
    let now = world.state.now();
    let signer = world.state.secrets.signer(&world.state.knot_did).unwrap();
    let created = CobStore::new(&git)
        .create(
            &knot_cob::CobHome::from(&repo_did),
            &CollaboratorsChange::Add(Grant {
                subject: account("olaren.dev"),
                added_by: account("olaren.dev"),
                created_at: now,
            }),
            &signer,
            now,
        )
        .unwrap();
    let oid = created.tip.oid();
    ["refs/heads/main", "refs/heads/trunk"]
        .into_iter()
        .for_each(|name| {
            git.update_ref(&RefUpdate::Create {
                name: RefName::new(name).unwrap(),
                new: oid,
            })
            .unwrap();
        });
    git.set_head(&RefName::new("refs/heads/main").unwrap())
        .unwrap();

    assert_eq!(
        as_member(
            &world,
            crate::branches::delete_branch,
            DELETE_BRANCH,
            json!({ "repo": repo_did, "branch": "trunk" })
        )
        .await
        .status(),
        StatusCode::OK,
        "a non-default branch is deleted"
    );
    assert!(
        git.find_ref(&RefName::new("refs/heads/trunk").unwrap())
            .unwrap()
            .is_none(),
        "trunk is gone"
    );
    assert_eq!(
        as_member(
            &world,
            crate::branches::delete_branch,
            DELETE_BRANCH,
            json!({ "repo": repo_did, "branch": "main" })
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST,
        "the current default branch cannot be deleted"
    );

    let event = only_git_event(&world);
    assert_eq!(event.nsid, "sh.tangled.git.refUpdate");
    assert_eq!(event.payload["repo"], repo_did.to_string());
    assert_eq!(event.payload["ref"], "refs/heads/trunk");
    assert_eq!(
        event.payload["oldSha"],
        oid.to_string(),
        "the deletion event includes the branch's old tip"
    );
    assert_eq!(
        event.payload["newSha"],
        git.object_format().null_oid().to_string(),
        "deletion reports the null oid as the new sha"
    );
    assert_eq!(
        event.payload["committerDid"],
        account(MEMBER_HOST).to_string()
    );
}

#[tokio::test]
async fn delete_repo_lifecycle_and_guards() {
    let world = World::new();
    add_member_helper(&world).await;

    let plain = create_repo_helper(&world, "whelk").await;
    assert_eq!(
        as_member(
            &world,
            crate::repos::delete_repo,
            DELETE,
            json!({ "repo": plain })
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert!(
        world.layout.open(&plain).is_err(),
        "bare repo is removed from disk"
    );
    assert_eq!(
        resolve(&world, "whelk"),
        Resolved::Ready(None),
        "repo is deregistered"
    );

    let guarded = create_repo_helper(&world, "conch").await;
    world.publish_pds_record("conch");
    let delete_conch = || json!({ "repo": guarded });
    assert_eq!(
        as_member(&world, crate::repos::delete_repo, DELETE, delete_conch())
            .await
            .status(),
        StatusCode::CONFLICT,
        "the guard refuses while the sh.tangled.repo record is still on the owner's PDS"
    );
    assert!(
        world.layout.open(&guarded).is_ok(),
        "a refused delete left the repo intact on disk"
    );

    let force_conch = || json!({ "repo": guarded, "force": true });
    assert_eq!(
        as_member(&world, crate::repos::delete_repo, DELETE, force_conch())
            .await
            .status(),
        StatusCode::FORBIDDEN,
        "force is an admin-only escape hatch instead of the owner's"
    );
    assert_eq!(
        as_admin(&world, crate::repos::delete_repo, DELETE, force_conch())
            .await
            .status(),
        StatusCode::OK,
        "a knot admin forces the delete past the lingering PDS record"
    );
    assert!(
        world.layout.open(&guarded).is_err(),
        "forced delete removed the repo from disk"
    );
}

#[tokio::test]
async fn rename_alias_lifecycle() {
    let world = World::new();
    add_member_helper(&world).await;

    let repo_a = create_repo_helper(&world, "alpha").await;
    assert_eq!(
        rename_repo_as(&world, &world.member, &repo_a, "alphanew").await,
        StatusCode::OK
    );
    assert_eq!(
        resolve(&world, "alphanew"),
        Resolved::Ready(Some(repo_a.clone())),
        "the new rkey resolves on the very next read"
    );
    assert_eq!(
        resolve(&world, "alpha"),
        Resolved::Ready(Some(repo_a.clone())),
        "the prior rkey keeps resolving as an alias"
    );
    assert_eq!(
        world.state.index.rkey_of(&repo_a),
        Resolved::Ready(Some(RepoRkey::new("alphanew").unwrap())),
        "the new rkey is canonical"
    );

    let repo_a2 = create_repo_helper(&world, "alpha").await;
    assert_ne!(repo_a2, repo_a, "a brand-new repo DID was minted");
    assert_eq!(
        resolve(&world, "alpha"),
        Resolved::Ready(Some(repo_a2.clone())),
        "the reused rkey now resolves to the new repo"
    );
    assert_eq!(
        resolve(&world, "alphanew"),
        Resolved::Ready(Some(repo_a.clone())),
        "the renamed repo keeps its canonical rkey"
    );

    let repo_b = create_repo_helper(&world, "beta").await;
    assert_eq!(
        rename_repo_as(&world, &world.member, &repo_b, "alpha").await,
        StatusCode::CONFLICT,
        "a rename cannot take the canonical rkey of another live repo"
    );
    assert_eq!(
        resolve(&world, "alpha"),
        Resolved::Ready(Some(repo_a2)),
        "the contested rkey still belongs to its original repo"
    );

    let repo_g = create_repo_helper(&world, "gamma").await;
    assert_eq!(
        rename_repo_as(&world, &world.member, &repo_g, "gammanew").await,
        StatusCode::OK
    );
    assert_eq!(
        as_member(
            &world,
            crate::repos::delete_repo,
            DELETE,
            json!({ "repo": repo_g })
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert!(
        world.layout.open(&repo_g).is_err(),
        "a renamed repo is deleted through its new rkey"
    );
    assert_eq!(
        resolve(&world, "gamma"),
        Resolved::Ready(None),
        "deletion drops the retained alias along with the repo"
    );

    let ghost = RepoDid::new("did:web:ghost.nel.pet").unwrap();
    assert_eq!(
        rename_repo_as(&world, &world.member, &ghost, "kelp").await,
        StatusCode::NOT_FOUND,
        "renaming a repo this knot doesn't host is 404 instead of 403"
    );
}

#[tokio::test]
async fn a_rename_against_a_warming_registry_is_unavailable_not_forbidden() {
    let admin = actor(1, ADMIN_HOST);
    let responder = doc_responder(admin.signer.public_key().as_bytes().to_vec(), || {
        StatusCode::OK
    });
    let (_dir, state) = build_state(responder, false);

    let token = mint(&admin, RENAME);
    assert_eq!(
        into_response(
            crate::repos::rename_repo(
                State(Arc::clone(&state)),
                bearer(&token),
                crate::Method::from_nsid(RENAME),
                body(json!({ "repo": "did:web:squid.nel.pet", "rkey": "kelp", "name": "kelp" })),
            )
            .await
        )
        .status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a warming registry is a retryable 503, never a permanent 403"
    );
}

#[tokio::test]
async fn the_router_sheds_a_pre_auth_flood_from_one_peer() {
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use std::net::SocketAddr;
    use tower::ServiceExt;

    let world = World::new();
    let app = crate::router(Arc::clone(&world.state));
    let peer = SocketAddr::from(([203, 0, 113, 7], 5555));

    let statuses: Vec<StatusCode> = futures::stream::iter(0..22)
        .then(|_| {
            let app = app.clone();
            async move {
                let mut request = http::Request::builder()
                    .method("POST")
                    .uri(crate::members::ADD_ROUTE)
                    .body(Body::empty())
                    .unwrap();
                request.extensions_mut().insert(ConnectInfo(peer));
                app.oneshot(request).await.unwrap().status()
            }
        })
        .collect()
        .await;

    assert!(
        statuses[..20]
            .iter()
            .all(|status| *status == StatusCode::UNAUTHORIZED),
        "per-peer burst is admitted and then fails auth on the missing token, got {statuses:?}"
    );
    assert!(
        statuses[20..]
            .iter()
            .all(|status| *status == StatusCode::TOO_MANY_REQUESTS),
        "past the burst the router sheds the flood before it can reach the resolver, got {statuses:?}"
    );
}

#[tokio::test]
async fn the_router_binds_each_route_to_its_matched_method_scope() {
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use std::net::SocketAddr;
    use tower::ServiceExt;

    let world = World::new();
    let app = crate::router(Arc::clone(&world.state));
    let peer = SocketAddr::from(([203, 0, 113, 23], 5555));

    let post = |token: String| {
        let mut request = http::Request::builder()
            .method("POST")
            .uri(crate::members::ADD_ROUTE)
            .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(
                serde_json::to_vec(&json!({ "subject": format!("did:web:{MEMBER_HOST}") }))
                    .unwrap(),
            ))
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(peer));
        request
    };

    let matched = mint(&world.admin, ADD_MEMBER);
    assert_eq!(
        app.clone().oneshot(post(matched)).await.unwrap().status(),
        StatusCode::OK,
        "a token whose lxm is the route's own method authenticates"
    );

    let sibling = mint(&world.admin, REMOVE_MEMBER);
    assert_eq!(
        app.oneshot(post(sibling)).await.unwrap().status(),
        StatusCode::UNAUTHORIZED,
        "a token minted for a sibling method is rejected at the addMember route"
    );
}

#[tokio::test]
async fn the_http_push_surface_sheds_a_bogus_credential_flood_from_one_peer() {
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use base64::Engine as _;
    use std::net::SocketAddr;
    use tower::ServiceExt;

    let world = World::new();
    add_member_helper(&world).await;
    let repo = create_repo_helper(&world, "kelp").await;
    let app = crate::router(Arc::clone(&world.state));
    let peer = SocketAddr::from(([203, 0, 113, 11], 5555));
    let credential = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("git:not-a-service-jwt")
    );

    let statuses: Vec<StatusCode> = futures::stream::iter(0..22)
        .then(|_| {
            let app = app.clone();
            let uri = format!("/{}/git-receive-pack", repo.as_str());
            let credential = credential.clone();
            async move {
                let mut request = http::Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header(http::header::AUTHORIZATION, credential)
                    .body(Body::empty())
                    .unwrap();
                request.extensions_mut().insert(ConnectInfo(peer));
                app.oneshot(request).await.unwrap().status()
            }
        })
        .collect()
        .await;

    assert!(
        statuses[..20]
            .iter()
            .all(|status| *status == StatusCode::UNAUTHORIZED),
        "bogus credentials inside the burst fail authentication, got {statuses:?}"
    );
    assert!(
        statuses[20..]
            .iter()
            .all(|status| *status == StatusCode::TOO_MANY_REQUESTS),
        "past the burst the push surface sheds the flood before it can reach the resolver, got {statuses:?}"
    );
}

#[tokio::test]
async fn health_is_unauthenticated_and_exempt_from_shedding() {
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use std::net::SocketAddr;
    use tower::ServiceExt;

    let world = World::new();
    let app = crate::router(Arc::clone(&world.state));
    let peer = SocketAddr::from(([203, 0, 113, 9], 5555));
    let health = || {
        let mut request = http::Request::builder()
            .method("GET")
            .uri(crate::service::HEALTH_ROUTE)
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(peer));
        request
    };

    let statuses = futures::future::join_all((0..30).map(|_| {
        let app = app.clone();
        async move { app.oneshot(health()).await.unwrap() }
    }))
    .await;
    assert!(
        statuses
            .iter()
            .all(|response| response.status() == StatusCode::OK),
        "health stays 200 even past the pre-auth burst, got {:?}",
        statuses.iter().map(|r| r.status()).collect::<Vec<_>>()
    );

    let wire = json_of(app.oneshot(health()).await.unwrap()).await;
    assert!(
        wire["version"]
            .as_str()
            .is_some_and(|v| v.starts_with("knot ")),
        "health reports a knot version, got {wire}"
    );
}

mod merge_endpoints {
    use super::*;
    use std::path::Path;

    use knot_git::{EntryKind, Identity, NewCommit, RefUpdate, StagedAction, StagedChange};
    use knot_types::{Oid, RefName, UnixSeconds};

    const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

    const UNIFIED_PATCH: &str = concat!(
        "diff --git a/reef.txt b/reef.txt\n",
        "index 1111111..2222222 100644\n",
        "--- a/reef.txt\n",
        "+++ b/reef.txt\n",
        "@@ -1 +1 @@\n",
        "-old line\n",
        "+new line\n",
    );

    const CONFLICTING_PATCH: &str = concat!(
        "diff --git a/reef.txt b/reef.txt\n",
        "index 1111111..2222222 100644\n",
        "--- a/reef.txt\n",
        "+++ b/reef.txt\n",
        "@@ -1 +1 @@\n",
        "-something else entirely\n",
        "+new line\n",
    );

    fn seed_main(world: &World, repo_did: &RepoDid, files: &[(&str, &str)]) -> Oid {
        let repo = world.layout.open(repo_did).unwrap();
        let staged: Vec<StagedChange> = files
            .iter()
            .map(|(path, content)| StagedChange {
                path: knot_types::RepoPath::new(*path).unwrap(),
                action: StagedAction::Put {
                    content: content.as_bytes().to_vec(),
                    kind: EntryKind::Blob,
                },
            })
            .collect();
        let tree = repo
            .write_staged_tree(Oid::from_hex(EMPTY_TREE).unwrap(), &staged)
            .unwrap();
        let nel = Identity {
            name: AuthorName::new("nel"),
            email: Email::new("nel@oyster.cafe"),
            time: UnixSeconds::new(1_000),
            offset_seconds: 0,
        };
        let commit = repo
            .write_commit(&NewCommit {
                tree,
                parents: Vec::new(),
                author: nel.clone(),
                committer: nel,
                message: "base".to_string(),
                extra_headers: Vec::new(),
            })
            .unwrap();
        repo.update_ref(&RefUpdate::Create {
            name: RefName::new("refs/heads/main").unwrap(),
            new: commit,
        })
        .unwrap();
        commit
    }

    fn main_tip(world: &World, repo_did: &RepoDid) -> Oid {
        world
            .layout
            .open(repo_did)
            .unwrap()
            .find_ref(&RefName::new("refs/heads/main").unwrap())
            .unwrap()
            .unwrap()
    }

    fn file_count(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| match entry.file_type() {
                        Ok(kind) if kind.is_dir() => file_count(&entry.path()),
                        _ => 1,
                    })
                    .sum()
            })
            .unwrap_or(0)
    }

    fn blob_at(repo: &knot_git::Repo, commit: Oid, path: &str) -> Vec<u8> {
        let entry = repo
            .entry_at(commit, &knot_types::RepoPath::new(path).unwrap())
            .unwrap()
            .unwrap();
        repo.read_blob(entry.oid).unwrap()
    }

    #[tokio::test]
    async fn the_owner_merges_a_unified_patch_natively_and_cleans_up() {
        let world = World::new();
        add_member_helper(&world).await;
        let repo_did = create_repo_helper(&world, "kelp").await;
        let base = seed_main(&world, &repo_did, &[("reef.txt", "old line\n")]);

        assert_eq!(
            as_member(
                &world,
                crate::merge::merge,
                MERGE,
                json!({
                    "repo": repo_did,
                    "branch": "main",
                    "patch": UNIFIED_PATCH,
                    "commitMessage": "Merge tide",
                    "commitBody": "body text",
                    "authorName": "bailey",
                    "authorEmail": "bailey@nel.pet",
                })
            )
            .await
            .status(),
            StatusCode::OK
        );

        let repo = world.layout.open(&repo_did).unwrap();
        let tip = main_tip(&world, &repo_did);
        assert_ne!(tip, base);
        let commit = repo.find_commit(tip).unwrap();
        assert_eq!(commit.parents, vec![base]);
        assert_eq!(commit.author.name.as_str(), "bailey");
        assert_eq!(commit.author.email.as_str(), "bailey@nel.pet");
        assert_eq!(commit.committer.name.as_str(), "Tangled");
        assert_eq!(commit.committer.email.as_str(), "noreply@tangled.sh");
        assert_eq!(commit.message, "Merge tide\n\nbody text\n");
        assert_eq!(blob_at(&repo, tip, "reef.txt"), b"new line\n");

        let event = only_git_event(&world);
        assert_eq!(event.nsid, "sh.tangled.git.refUpdate");
        assert_eq!(event.payload["repo"], repo_did.to_string());
        assert_eq!(event.payload["ref"], "refs/heads/main");
        assert_eq!(event.payload["oldSha"], base.to_string());
        assert_eq!(event.payload["newSha"], tip.to_string());
        assert_eq!(
            event.payload["committerDid"],
            account(MEMBER_HOST).to_string()
        );

        let staging = std::fs::read_dir(repo.path())
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(knot_git::INCOMING_PREFIX))
            })
            .count();
        assert_eq!(
            staging, 0,
            "a completed merge cleans up its staging directory"
        );
    }

    #[tokio::test]
    async fn a_native_merge_advances_the_branch_without_a_pipeline_event() {
        let world = World::new();
        add_member_helper(&world).await;
        let repo_did = create_repo_helper(&world, "kelp").await;
        seed_main(
            &world,
            &repo_did,
            &[
                ("reef.txt", "old line\n"),
                (
                    ".tangled/workflows/ci.yml",
                    "engine: nixery.dev/x\nwhen:\n  - event: push\n    branch: ['**']\n",
                ),
            ],
        );

        assert_eq!(
            as_member(
                &world,
                crate::merge::merge,
                MERGE,
                json!({
                    "repo": repo_did,
                    "branch": "main",
                    "patch": UNIFIED_PATCH,
                    "commitMessage": "Merge tide",
                    "authorName": "bailey",
                    "authorEmail": "bailey@nel.pet",
                })
            )
            .await
            .status(),
            StatusCode::OK
        );

        let update = last_event(&world, "sh.tangled.git.refUpdate");
        assert_eq!(update.payload["ref"], "refs/heads/main");
        assert!(
            replay(&world)
                .iter()
                .all(|event| event.nsid != "sh.tangled.pipeline"),
            "the knot emits no pipeline record"
        );
    }

    #[tokio::test]
    async fn a_format_patch_merge_creates_one_commit_per_patch_with_change_id() {
        let world = World::new();
        add_member_helper(&world).await;
        let repo_did = create_repo_helper(&world, "limpet").await;
        let base = seed_main(&world, &repo_did, &[("reef.txt", "one\n")]);

        let mbox = concat!(
            "From 1111111111111111111111111111111111111111 Mon Sep 17 00:00:00 2001\n",
            "From: olaren <olaren@olaren.dev>\n",
            "Date: Tue, 5 Sep 2023 12:00:00 +0530\n",
            "Subject: [PATCH 1/2] first step\n",
            "\n",
            "step one body\n",
            "---\n",
            " reef.txt | 2 +-\n",
            "\n",
            "diff --git a/reef.txt b/reef.txt\n",
            "index 1111111..2222222 100644\n",
            "--- a/reef.txt\n",
            "+++ b/reef.txt\n",
            "@@ -1 +1 @@\n",
            "-one\n",
            "+two\n",
            "-- \n2.43.0\n\n",
            "From 2222222222222222222222222222222222222222 Mon Sep 17 00:00:00 2001\n",
            "From: olaren <olaren@olaren.dev>\n",
            "Date: Tue, 5 Sep 2023 13:00:00 +0530\n",
            "Subject: [PATCH 2/2] second step\n",
            "Change-Id: Ifeedfacecafe\n",
            "\n",
            "---\n",
            "diff --git a/reef.txt b/reef.txt\n",
            "index 2222222..3333333 100644\n",
            "--- a/reef.txt\n",
            "+++ b/reef.txt\n",
            "@@ -1 +1 @@\n",
            "-two\n",
            "+three\n",
        );

        assert_eq!(
            as_member(
                &world,
                crate::merge::merge,
                MERGE,
                json!({
                    "repo": repo_did,
                    "branch": "main",
                    "patch": mbox,
                })
            )
            .await
            .status(),
            StatusCode::OK
        );

        let repo = world.layout.open(&repo_did).unwrap();
        let tip = main_tip(&world, &repo_did);
        let second = repo.find_commit(tip).unwrap();
        assert_eq!(second.message, "second step\n");
        assert_eq!(
            second.change_id(),
            Some(knot_git::CommitChangeId::new("Ifeedfacecafe").unwrap())
        );
        assert_eq!(second.author.name.as_str(), "olaren");
        assert_eq!(second.author.email.as_str(), "olaren@olaren.dev");
        assert_eq!(second.author.time.get(), 1_693_899_000);
        assert_eq!(second.author.offset_seconds, 19_800);
        assert_eq!(second.committer.name.as_str(), "Tangled");
        assert_eq!(second.committer.email.as_str(), "noreply@tangled.sh");

        let first = repo.find_commit(second.parents[0]).unwrap();
        assert_eq!(first.message, "first step\n\nstep one body\n");
        assert_eq!(first.author.time.get(), 1_693_895_400);
        assert_eq!(first.parents, vec![base]);
        assert_eq!(blob_at(&repo, tip, "reef.txt"), b"three\n");
    }

    #[tokio::test]
    async fn merge_rejections_move_nothing() {
        let world = World::new();
        add_member_helper(&world).await;
        let repo_did = create_repo_helper(&world, "scallop").await;
        let base = seed_main(&world, &repo_did, &[("reef.txt", "old line\n")]);

        assert_eq!(
            as_stranger(
                &world,
                crate::merge::merge,
                MERGE,
                json!({ "repo": repo_did, "branch": "main", "patch": UNIFIED_PATCH })
            )
            .await
            .status(),
            StatusCode::FORBIDDEN,
            "a stranger cannot merge"
        );

        assert_eq!(
            as_member(
                &world,
                crate::merge::merge,
                MERGE,
                json!({ "repo": repo_did, "branch": "main", "patch": UNIFIED_PATCH })
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST,
            "a merge without a commit message is rejected"
        );
        assert_eq!(
            main_tip(&world, &repo_did),
            base,
            "rejected merge mustn't move the branch"
        );

        assert_eq!(
            as_member(&world, crate::merge::merge, MERGE, json!({ "repo": repo_did, "branch": "driftwood", "patch": UNIFIED_PATCH, "commitMessage": "tide" })).await.status(),
            StatusCode::BAD_REQUEST,
            "merging into a branch the repo lacks is an invalid request"
        );

        let response = as_member(&world, crate::merge::merge, MERGE, json!({ "repo": repo_did, "branch": "main", "patch": CONFLICTING_PATCH, "commitMessage": "tide" })).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let json = json_of(response).await;
        assert_eq!(json["error"], "MergeConflict");
        assert!(
            json["message"]
                .as_str()
                .unwrap()
                .starts_with("Merge failed due to conflicts"),
        );
        assert_eq!(
            main_tip(&world, &repo_did),
            base,
            "a conflicted merge mustn't move the branch"
        );
    }

    #[tokio::test]
    async fn merge_check_is_open_and_reports_clean_conflicted_and_broken() {
        let world = World::new();
        add_member_helper(&world).await;
        let repo_did = create_repo_helper(&world, "scallop").await;
        let base = seed_main(&world, &repo_did, &[("reef.txt", "old line\n")]);

        let repo = world.layout.open(&repo_did).unwrap();
        let objects_before = file_count(&repo.objects_dir());

        let input = |patch: &str| {
            body(json!({
                "repo": repo_did,
                "branch": "main",
                "patch": patch,
            }))
        };

        let clean = json_of(
            crate::merge::merge_check(world.state(), input(UNIFIED_PATCH))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(clean["is_conflicted"], false);
        assert!(clean.get("conflicts").is_none());

        let conflicted = json_of(
            crate::merge::merge_check(world.state(), input(CONFLICTING_PATCH))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(conflicted["is_conflicted"], true);
        assert_eq!(conflicted["conflicts"][0]["filename"], "reef.txt");
        assert_eq!(conflicted["conflicts"][0]["reason"], "patch doesn't apply");
        assert_eq!(conflicted["message"], "patch cannot be applied cleanly");

        let broken = json_of(
            crate::merge::merge_check(world.state(), input("hello world\n"))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(broken["is_conflicted"], true);
        assert!(broken["error"].as_str().is_some());

        assert_eq!(
            file_count(&repo.objects_dir()),
            objects_before,
            "merge check must write nothing into the object database"
        );
        assert_eq!(main_tip(&world, &repo_did), base);
    }

    #[tokio::test]
    async fn a_repo_did_addresses_a_repo_whose_rkey_differs_from_its_name() {
        let world = World::new();
        add_member_helper(&world).await;
        assert_eq!(
            as_member(
                &world,
                crate::repos::create_repo,
                CREATE,
                json!({ "rkey": "3mjmslfzgwb22", "name": "periwinkle" })
            )
            .await
            .status(),
            StatusCode::OK
        );
        let repo_did = match resolve(&world, "3mjmslfzgwb22") {
            Resolved::Ready(Some(did)) => did,
            other => panic!("repo wasn't registered: {other:?}"),
        };
        let base = seed_main(&world, &repo_did, &[("reef.txt", "old line\n")]);

        let rejections = [
            (
                json!({ "did": format!("did:web:{MEMBER_HOST}"), "name": "periwinkle", "branch": "main", "patch": UNIFIED_PATCH }),
                StatusCode::BAD_REQUEST,
                "we'll 400 an owner and a name instead of a repo DID",
            ),
            (
                json!({ "repo": "did:plc:limpet", "branch": "main", "patch": UNIFIED_PATCH }),
                StatusCode::NOT_FOUND,
                "we'll 404 a repo DID that this knot doesn't host",
            ),
            (
                json!({ "repo": "periwinkle", "branch": "main", "patch": UNIFIED_PATCH }),
                StatusCode::BAD_REQUEST,
                "we'll 400 a repo that isn't a DID",
            ),
        ];
        for (input, want, why) in rejections {
            let response =
                into_response(crate::merge::merge_check(world.state(), body(input)).await);
            assert_eq!(response.status(), want, "{why}");
        }

        assert_eq!(
            as_member(
                &world,
                crate::merge::merge,
                MERGE,
                json!({ "repo": repo_did, "branch": "main", "patch": UNIFIED_PATCH, "commitMessage": "tide" })
            )
            .await
            .status(),
            StatusCode::OK
        );
        assert_ne!(main_tip(&world, &repo_did), base);
    }
}

mod fork_endpoints {
    use super::*;

    use knot_git::{EntryKind, Identity, NewCommit, RefUpdate, Repo, StagedAction, StagedChange};
    use knot_runtime::HttpTransport;
    use knot_types::{Oid, RefName, UnixSeconds};

    const EMPTY_TREE_SHA1: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    const EMPTY_TREE_SHA256: &str =
        "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";

    fn empty_tree(repo: &Repo) -> Oid {
        let hex = match repo.object_format() {
            knot_types::ObjectFormat::SHA256 => EMPTY_TREE_SHA256,
            _ => EMPTY_TREE_SHA1,
        };
        Oid::from_hex(hex).unwrap()
    }

    fn ident(time: i64) -> Identity {
        Identity {
            name: AuthorName::new("nel"),
            email: Email::new("nel@oyster.cafe"),
            time: UnixSeconds::new(time),
            offset_seconds: 0,
        }
    }

    fn put_commit(repo: &Repo, parent: Option<Oid>, path: &str, content: &str, time: i64) -> Oid {
        let base_tree = match parent {
            Some(parent) => repo.find_commit(parent).unwrap().tree,
            None => empty_tree(repo),
        };
        let staged = vec![StagedChange {
            path: knot_types::RepoPath::new(path).unwrap(),
            action: StagedAction::Put {
                content: content.as_bytes().to_vec(),
                kind: EntryKind::Blob,
            },
        }];
        let tree = repo.write_staged_tree(base_tree, &staged).unwrap();
        repo.write_commit(&NewCommit {
            tree,
            parents: parent.into_iter().collect(),
            author: ident(time),
            committer: ident(time),
            message: format!("put {path}"),
            extra_headers: Vec::new(),
        })
        .unwrap()
    }

    fn advance(repo: &Repo, branch: &RefName, path: &str, content: &str, time: i64) -> Oid {
        let old = repo.find_ref(branch).unwrap();
        let new = put_commit(repo, old, path, content, time);
        let update = match old {
            Some(old) => RefUpdate::Update {
                name: branch.clone(),
                old,
                new,
            },
            None => RefUpdate::Create {
                name: branch.clone(),
                new,
            },
        };
        repo.update_ref(&update).unwrap();
        new
    }

    fn main_ref() -> RefName {
        RefName::new("refs/heads/main").unwrap()
    }

    fn member_did() -> OwnerDid {
        OwnerDid::new(format!("did:web:{MEMBER_HOST}")).unwrap()
    }

    fn source_url(rkey: &str) -> String {
        format!("https://{KNOT_HOST}/did:web:{MEMBER_HOST}/{rkey}")
    }

    async fn fork_repo(world: &World, source: &str, rkey: &str, name: &str) -> RepoDid {
        assert_eq!(
            as_member(
                world,
                crate::repos::create_repo,
                CREATE,
                json!({ "rkey": rkey, "name": name, "source": source })
            )
            .await
            .status(),
            StatusCode::OK
        );
        match world
            .state
            .index
            .resolve_repo(&member_did(), &RepoRkey::new(rkey).unwrap())
        {
            Resolved::Ready(Some(did)) => did,
            other => panic!("fork {rkey} wasn't registered: {other:?}"),
        }
    }

    struct ForkWorld {
        world: World,
        source_did: RepoDid,
        fork_did: RepoDid,
        tip: Oid,
    }

    async fn forked_world() -> ForkWorld {
        let world = World::new();
        add_member_helper(&world).await;
        let source_did = create_repo_helper(&world, "kelp").await;
        let source = world.layout.open(&source_did).unwrap();
        advance(&source, &main_ref(), "reef.txt", "kelp forest\n", 1_000);
        let tip = advance(&source, &main_ref(), "tide.txt", "rock pool\n", 1_001);
        [
            "refs/tags/v1",
            "refs/cobs/sh.tangled.repo.collaborator/limpet",
            "refs/hidden/feature/main",
        ]
        .into_iter()
        .for_each(|name| {
            source
                .update_ref(&RefUpdate::Create {
                    name: RefName::new(name).unwrap(),
                    new: tip,
                })
                .unwrap();
        });
        let fork_did = fork_repo(&world, &source_url("kelp"), "uni", "uni").await;
        ForkWorld {
            world,
            source_did,
            fork_did,
            tip,
        }
    }

    async fn sync_fork(
        world: &World,
        actor: &Actor,
        fork_did: &RepoDid,
        branch: &str,
    ) -> StatusCode {
        call(
            world,
            crate::forks::fork_sync,
            actor,
            FORK_SYNC,
            json!({ "repo": fork_did, "branch": branch }),
        )
        .await
        .status()
    }

    async fn track_hidden(
        world: &World,
        fork_did: &RepoDid,
        fork_ref: &str,
        remote_ref: &str,
    ) -> StatusCode {
        as_member(
            world,
            crate::forks::hidden_ref,
            HIDDEN_REF,
            json!({
                "repo": fork_did,
                "forkRef": fork_ref,
                "remoteRef": remote_ref,
            }),
        )
        .await
        .status()
    }

    #[tokio::test]
    async fn a_member_forks_a_repo_hosted_on_this_knot() {
        let setup = forked_world().await;
        assert_ne!(setup.fork_did, setup.source_did);

        let fork = setup.world.layout.open(&setup.fork_did).unwrap();
        assert_eq!(fork.find_ref(&main_ref()).unwrap(), Some(setup.tip));
        assert_eq!(
            fork.find_ref(&RefName::new("refs/tags/v1").unwrap())
                .unwrap(),
            Some(setup.tip)
        );
        assert_eq!(fork.default_branch().unwrap().as_str(), "refs/heads/main");
        assert_eq!(fork.origin_url(), Some(OriginUrl::new(source_url("kelp"))));
        assert!(
            fork.references().unwrap().iter().all(|record| {
                !record.name.as_str().starts_with("refs/cobs/")
                    && !record.name.as_str().starts_with("refs/hidden/")
            }),
            "a fork must copy only heads and tags, never cob or hidden refs"
        );

        let entry = fork
            .entry_at(setup.tip, &knot_types::RepoPath::new("tide.txt").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(fork.read_blob(entry.oid).unwrap(), b"rock pool\n");
    }

    #[tokio::test]
    async fn forking_a_source_this_knot_does_not_host_is_not_found() {
        let world = World::new();
        add_member_helper(&world).await;
        assert_eq!(
            as_member(&world, crate::repos::create_repo, CREATE, json!({ "rkey": "uni", "name": "uni", "source": format!("https://{KNOT_HOST}/did:plc:whelk/ghost") })).await.status(),
            StatusCode::NOT_FOUND
        );
        assert!(matches!(
            world
                .state
                .index
                .resolve_repo(&member_did(), &RepoRkey::new("uni").unwrap()),
            Resolved::Ready(None)
        ));
    }

    #[tokio::test]
    async fn fork_sync_lifecycle() {
        let setup = forked_world().await;
        let source = setup.world.layout.open(&setup.source_did).unwrap();
        let new_tip = advance(&source, &main_ref(), "spray.txt", "salt\n", 1_002);

        assert_eq!(
            sync_fork(&setup.world, &setup.world.member, &setup.fork_did, "main").await,
            StatusCode::OK
        );
        let fork = setup.world.layout.open(&setup.fork_did).unwrap();
        assert_eq!(fork.find_ref(&main_ref()).unwrap(), Some(new_tip));

        let event = only_git_event(&setup.world);
        assert_eq!(event.nsid, "sh.tangled.git.refUpdate");
        assert_eq!(event.payload["repo"], setup.fork_did.to_string());
        assert_eq!(event.payload["ref"], "refs/heads/main");
        assert_eq!(event.payload["oldSha"], setup.tip.to_string());
        assert_eq!(event.payload["newSha"], new_tip.to_string());
        assert_eq!(
            event.payload["committerDid"],
            account(MEMBER_HOST).to_string()
        );

        assert_eq!(
            sync_fork(&setup.world, &setup.world.member, &setup.fork_did, "main").await,
            StatusCode::OK,
            "an up-to-date sync is a no-op"
        );
        assert_eq!(
            git_events(&setup.world).len(),
            1,
            "an up-to-date sync emits no further event"
        );

        assert_eq!(
            sync_fork(&setup.world, &setup.world.stranger, &setup.fork_did, "main").await,
            StatusCode::FORBIDDEN,
            "a stranger cannot sync a fork"
        );
        assert_eq!(
            sync_fork(
                &setup.world,
                &setup.world.member,
                &setup.fork_did,
                "driftwood"
            )
            .await,
            StatusCode::NOT_FOUND,
            "syncing a branch the upstream lacks isn't found"
        );
    }

    #[tokio::test]
    async fn a_repo_did_addresses_a_fork_whose_rkey_differs_from_its_name() {
        let world = World::new();
        add_member_helper(&world).await;
        let source_did = create_repo_helper(&world, "kelp").await;
        let source = world.layout.open(&source_did).unwrap();
        advance(&source, &main_ref(), "reef.txt", "kelp forest\n", 1_000);

        let fork_did = fork_repo(&world, &source_url("kelp"), "3mjmslfzgwb22", "nautilus").await;

        let new_tip = advance(&source, &main_ref(), "spray.txt", "salt\n", 1_001);
        assert_eq!(
            sync_fork(&world, &world.member, &fork_did, "main").await,
            StatusCode::OK
        );
        assert_eq!(
            world
                .layout
                .open(&fork_did)
                .unwrap()
                .find_ref(&main_ref())
                .unwrap(),
            Some(new_tip),
            "the fork will sync by repo DID, which the name never has to match"
        );
    }

    #[tokio::test]
    async fn hidden_ref_tracks_the_upstream_branch_and_stays_hidden() {
        let setup = forked_world().await;
        let source = setup.world.layout.open(&setup.source_did).unwrap();
        let new_tip = advance(&source, &main_ref(), "spray.txt", "salt\n", 1_002);

        assert_eq!(
            track_hidden(&setup.world, &setup.fork_did, "feature", "main").await,
            StatusCode::OK
        );

        let fork = setup.world.layout.open(&setup.fork_did).unwrap();
        let hidden = RefName::new("refs/hidden/feature/main").unwrap();
        assert_eq!(fork.find_ref(&hidden).unwrap(), Some(new_tip));
        assert!(
            fork.advertised_refs()
                .unwrap()
                .iter()
                .all(|record| !record.name.as_str().starts_with("refs/hidden/")),
            "a hidden ref must stay out of the public advertisement"
        );
        assert_eq!(
            track_hidden(&setup.world, &setup.fork_did, "feature", "main").await,
            StatusCode::OK,
            "tracking an already-tracked ref is idempotent"
        );
    }

    #[tokio::test]
    async fn hidden_ref_resolves_a_file_origin_by_trailing_segments() {
        let setup = forked_world().await;
        let source = setup.world.layout.open(&setup.source_did).unwrap();
        let hidden = RefName::new("refs/hidden/feature/main").unwrap();

        setup
            .world
            .layout
            .open(&setup.fork_did)
            .unwrap()
            .set_origin_url(&OriginUrl::new(format!(
                "file:///home/git/{}",
                setup.source_did.as_str()
            )))
            .unwrap();
        let did_tip = advance(&source, &main_ref(), "spray.txt", "salt\n", 1_002);
        assert_eq!(
            track_hidden(&setup.world, &setup.fork_did, "feature", "main").await,
            StatusCode::OK
        );
        assert_eq!(
            setup
                .world
                .layout
                .open(&setup.fork_did)
                .unwrap()
                .find_ref(&hidden)
                .unwrap(),
            Some(did_tip),
            "a trailing repo did resolves to the source repo"
        );

        setup
            .world
            .layout
            .open(&setup.fork_did)
            .unwrap()
            .set_origin_url(&OriginUrl::new(format!(
                "file:///home/git/did:web:{MEMBER_HOST}/kelp"
            )))
            .unwrap();
        let named_tip = advance(&source, &main_ref(), "swell.txt", "tide\n", 1_003);
        assert_eq!(
            track_hidden(&setup.world, &setup.fork_did, "feature", "main").await,
            StatusCode::OK
        );
        assert_eq!(
            setup
                .world
                .layout
                .open(&setup.fork_did)
                .unwrap()
                .find_ref(&hidden)
                .unwrap(),
            Some(named_tip),
            "a trailing owner and name resolves to the source repo"
        );
    }

    #[tokio::test]
    async fn hidden_ref_rejects_a_stale_or_non_http_stored_origin() {
        let setup = forked_world().await;
        let fork = setup.world.layout.open(&setup.fork_did).unwrap();

        fork.set_origin_url(&OriginUrl::new("file:///home/git/did:plc:whelk"))
            .unwrap();
        assert_eq!(
            track_hidden(&setup.world, &setup.fork_did, "feature", "main").await,
            StatusCode::NOT_FOUND,
            "the knot reports not found for a file origin with an unknown repo did"
        );

        fork.set_origin_url(&OriginUrl::new("ssh://knot.nel.pet/did:plc:whelk/ghost"))
            .unwrap();
        assert_eq!(
            track_hidden(&setup.world, &setup.fork_did, "feature", "main").await,
            StatusCode::INTERNAL_SERVER_ERROR,
            "the knot reports an internal error for a stored origin scheme other than http, https, or file"
        );

        fork.set_origin_url(&OriginUrl::new("file:///kelp"))
            .unwrap();
        assert_eq!(
            track_hidden(&setup.world, &setup.fork_did, "feature", "main").await,
            StatusCode::INTERNAL_SERVER_ERROR,
            "the knot reports an internal error for a file origin whose only path segment isn't a DID"
        );
    }

    #[test]
    fn parse_trailing_resolves_each_stored_path_shape() {
        let shape = |raw: &str| {
            let url = url::Url::parse(raw).unwrap();
            match crate::forks::LocalPath::parse_trailing(&url) {
                Ok(crate::forks::LocalPath::Did(did)) => format!("did {did}"),
                Ok(crate::forks::LocalPath::Named { owner, name }) => {
                    format!("named {owner} {name}")
                }
                Err(reason) => format!("err {reason}"),
            }
        };
        [
            (
                "file:///home/git/did:web:oyster.cafe/kelp",
                "named did:web:oyster.cafe kelp",
            ),
            (
                "file:///home/git/did:web:oyster.cafe/kelp.git",
                "named did:web:oyster.cafe kelp",
            ),
            (
                "file:///home/git/did:web:oyster.cafe/did:plc:squid",
                "named did:web:oyster.cafe did:plc:squid",
            ),
            ("file:///data/repos/did:plc:squid", "did did:plc:squid"),
            ("file:///did:plc:squid", "did did:plc:squid"),
            (
                "file:///home/git/kelp",
                "err path ends in neither /owner-did/name or /repo-did",
            ),
            ("file:///kelp", "err path isn't a DID"),
            ("file:///", "err path must be /did or /owner/name"),
        ]
        .into_iter()
        .for_each(|(raw, expected)| assert_eq!(shape(raw), expected, "{raw}"));
    }

    #[tokio::test]
    async fn a_fork_over_http_takes_the_upstream_object_format_and_conflicts_when_it_changes() {
        let upstream_dir = tempfile::tempdir().unwrap();
        let upstream_path = upstream_dir.path().join("uni.git");
        let upstream =
            Repo::create_with_format(&upstream_path, knot_types::ObjectFormat::SHA1).unwrap();
        upstream.set_head(&main_ref()).unwrap();
        advance(&upstream, &main_ref(), "reef.txt", "kelp forest\n", 1_000);
        let tip = advance(&upstream, &main_ref(), "tide.txt", "rock pool\n", 1_001);

        let served = Arc::new(std::sync::RwLock::new(upstream_path.clone()));
        let path = Arc::clone(&served);
        let git_http: Arc<dyn HttpTransport> =
            Arc::new(FakeHttp::new(move |request: &HttpRequest| {
                let repo = Repo::open(path.read().unwrap().clone()).unwrap();
                let body = if request.url.path().ends_with("/info/refs") {
                    knot_pack::advertise_upload(&repo).unwrap()
                } else {
                    knot_pack::upload_pack(&repo, request.body.as_deref().unwrap_or_default())
                        .unwrap()
                };
                Ok(HttpResponse {
                    status: StatusCode::OK,
                    headers: http::HeaderMap::new(),
                    body: body.into(),
                })
            }));

        let world = World::with_git_http(git_http, knot_types::ObjectFormat::SHA256);
        add_member_helper(&world).await;
        let plain = create_repo_helper(&world, "kelp").await;
        assert_eq!(
            world.layout.open(&plain).unwrap().object_format(),
            knot_types::ObjectFormat::SHA256,
            "a repo with no upstream still uses this knot's configured format"
        );

        let remote = "https://barnacle.nel.pet/did:plc:squid/uni";
        let fork_did = fork_repo(&world, remote, "uni", "uni").await;
        let fork = world.layout.open(&fork_did).unwrap();
        assert_eq!(
            fork.object_format(),
            knot_types::ObjectFormat::SHA1,
            "a fork of a sha1 upstream must be sha1 so the upstream's objects ingest"
        );
        assert_eq!(fork.find_ref(&main_ref()).unwrap(), Some(tip));
        assert_eq!(fork.origin_url(), Some(OriginUrl::new(remote)));
        assert_eq!(
            fork.read_blob(
                fork.entry_at(tip, &knot_types::RepoPath::new("tide.txt").unwrap())
                    .unwrap()
                    .unwrap()
                    .oid
            )
            .unwrap(),
            b"rock pool\n"
        );

        let new_tip = advance(
            &Repo::open(&upstream_path).unwrap(),
            &main_ref(),
            "spray.txt",
            "salt\n",
            1_002,
        );
        assert_eq!(
            sync_fork(&world, &world.member, &fork_did, "main").await,
            StatusCode::OK
        );
        assert_eq!(
            world
                .layout
                .open(&fork_did)
                .unwrap()
                .find_ref(&main_ref())
                .unwrap(),
            Some(new_tip)
        );

        let replaced = upstream_dir.path().join("replaced.git");
        let sha256 = Repo::create_with_format(&replaced, knot_types::ObjectFormat::SHA256).unwrap();
        sha256.set_head(&main_ref()).unwrap();
        advance(&sha256, &main_ref(), "reef.txt", "kelp forest\n", 1_000);
        *served.write().unwrap() = replaced;
        assert_eq!(
            sync_fork(&world, &world.member, &fork_did, "main").await,
            StatusCode::CONFLICT,
            "the fork reports a format mismatch as a conflict"
        );
    }
}

mod legacy_admin_route {
    use super::*;
    use crate::legacy_admin::{ADD_MEMBER_ROUTE, LegacyAdminSecret};
    use tower::ServiceExt;

    const SECRET: &str = "nekomilk2";

    async fn call(router: &axum::Router, user: &str, password: &str, subject: &str) -> StatusCode {
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(format!("{user}:{password}").as_bytes());
        let request = http::Request::builder()
            .method("POST")
            .uri(ADD_MEMBER_ROUTE)
            .header(AUTHORIZATION, format!("Basic {encoded}"))
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(
                json!({ "subject": subject }).to_string(),
            ))
            .unwrap();
        router.clone().oneshot(request).await.unwrap().status()
    }

    #[tokio::test]
    async fn the_legacy_route_admits_a_member_only_with_the_configured_credentials() {
        let world = World::new();
        let router = crate::router(Arc::clone(&world.state)).merge(crate::legacy_admin::router(
            Arc::clone(&world.state),
            LegacyAdminSecret::new(SECRET).unwrap(),
        ));
        let subject = format!("did:web:{MEMBER_HOST}");

        let version = http::Request::builder()
            .method("GET")
            .uri(crate::service::VERSION_ROUTE)
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            router.clone().oneshot(version).await.unwrap().status(),
            StatusCode::OK,
            "merging the legacy route leaves the xrpc routes reachable"
        );

        let refused = futures::future::join_all(
            [("admin", "nope"), ("root", SECRET), ("admin", "")]
                .map(|(user, password)| call(&router, user, password, &subject)),
        )
        .await;
        assert!(
            refused
                .iter()
                .all(|status| *status == StatusCode::UNAUTHORIZED),
            "the knot refuses a wrong user or secret, got {refused:?}"
        );
        assert_eq!(
            call(
                &router,
                "admin",
                SECRET,
                &"n".repeat(world.state.byte_limits.body.get() + 1)
            )
            .await,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            world.state.index.is_member(&account(MEMBER_HOST)),
            Resolved::Ready(false),
            "a refused call grants nothing"
        );

        assert_eq!(
            call(&router, "admin", SECRET, &subject).await,
            StatusCode::OK
        );
        assert_eq!(
            world.state.index.is_member(&account(MEMBER_HOST)),
            Resolved::Ready(true)
        );
        let added = last_event(&world, "sh.tangled.knot.memberUpdate");
        assert_eq!(added.payload["op"], "add");
        assert_eq!(added.payload["subject"], account(MEMBER_HOST).to_string());
        let Resolved::Ready(members) = world.state.index.member_entries() else {
            panic!("the member roster is warm in this test");
        };
        assert_eq!(
            members
                .iter()
                .find(|grant| grant.subject == account(MEMBER_HOST))
                .expect("the member is in the roster")
                .added_by,
            world.state.service_owner,
            "the legacy grant records the service owner as the granter"
        );

        let baseline = event_count(&world);
        assert_eq!(
            call(&router, "admin", SECRET, &subject).await,
            StatusCode::OK,
            "the legacy route is idempotent, matching the Go knot"
        );
        assert_eq!(
            event_count(&world),
            baseline,
            "re-adding an existing member emits no event"
        );
    }

    #[tokio::test]
    async fn the_legacy_route_sheds_a_pre_auth_flood_from_one_peer() {
        use axum::extract::ConnectInfo;
        use std::net::SocketAddr;

        let world = World::new();
        let router = crate::legacy_admin::router(
            Arc::clone(&world.state),
            LegacyAdminSecret::new(SECRET).unwrap(),
        );
        let peer = SocketAddr::from(([203, 0, 113, 9], 5555));

        let statuses: Vec<StatusCode> = futures::stream::iter(0..22)
            .then(|_| {
                let router = router.clone();
                async move {
                    let mut request = http::Request::builder()
                        .method("POST")
                        .uri(ADD_MEMBER_ROUTE)
                        .body(axum::body::Body::empty())
                        .unwrap();
                    request.extensions_mut().insert(ConnectInfo(peer));
                    router.oneshot(request).await.unwrap().status()
                }
            })
            .collect()
            .await;

        assert!(
            statuses[..20]
                .iter()
                .all(|status| *status == StatusCode::UNAUTHORIZED),
            "the knot admits the per-peer burst and then fails it on the missing credentials, got {statuses:?}"
        );
        assert!(
            statuses[20..]
                .iter()
                .all(|status| *status == StatusCode::TOO_MANY_REQUESTS),
            "past the burst the knot sheds the guess flood before it reaches the secret comparison, got {statuses:?}"
        );
    }

    #[test]
    fn every_wire_copy_of_an_embedded_payload_fits_a_quarter_of_the_response() {
        let limits = crate::ByteLimits::default();
        assert_eq!(limits.response.get(), 5 * 1024 * 1024);
        let response = limits.response.get() as u64;
        assert_eq!(
            limits.binary_patch(),
            knot_git::BinaryBudget::new(response / 4 / 3),
            "a compare serialises the series twice and the combined patch once"
        );
        let per_pass = match limits.binary_patch() {
            knot_git::BinaryBudget::Spend { remaining, .. } => remaining,
            knot_git::BinaryBudget::Omit => panic!("binary_patch spends, it doesn't omit"),
        };
        assert!(
            per_pass * 3 <= response / 4,
            "three copies of {per_pass} bytes must stay inside a quarter of {response}"
        );
    }
}
