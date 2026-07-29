#![allow(dead_code)]

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::{Body, Bytes};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::{HeaderMap, StatusCode, header};
use k256::ecdsa::signature::Signer;
use k256::ecdsa::{Signature, SigningKey};
use tower::ServiceExt;

use knot_atproto::Atproto;
use knot_cob::{CobHome, CobStore};
use knot_cobs::{
    CollaboratorsChange, CollaboratorsCob, Grant, MembersChange, MembersCob, Registration,
    RegistryChange, RepoRegistryCob, register_repo,
};
use knot_git::{Layout, Repo};
use knot_index::Index;
use knot_runtime::{
    FakeHttp, HttpRequest, HttpResponse, ManualClock, NetworkError, OsEntropy, UnixMicros,
};
use knot_secrets::{MasterKey, SealedStore};
use knot_types::{
    AccountDid, AuthorName, Email, KnotHostname, KnotId, ObjectFormat, Oid, OwnerDid, RepoDid,
    RepoName, RepoRkey, UnixSeconds,
};
use knot_xrpc::{
    ArchiveLimit, Budgets, ByteLimits, CobLocks, GlobalQuota, LimitConfig, PerActorQuota,
    PreAuthLimiter, Reservations, ResponseLimit, XrpcState,
};

pub const KNOT_HOST: &str = "knot.nel.pet";
pub const OWNER: &str = "did:web:olaren.dev";

pub type Responder = Box<dyn Fn(&HttpRequest) -> Result<HttpResponse, NetworkError> + Send + Sync>;

pub struct World {
    _dir: tempfile::TempDir,
    pub layout: Layout,
    pub lfs_dir: std::path::PathBuf,
    pub router: Router,
    pub state: Arc<XrpcState<FakeHttp<Responder>, ManualClock>>,
}

impl World {
    pub fn new() -> Self {
        Self::build(true, ByteLimits::default(), ObjectFormat::SHA1)
    }

    pub fn unshed() -> Self {
        Self::build_with_limits(
            true,
            ByteLimits::default(),
            ObjectFormat::SHA1,
            LimitConfig::unmetered(),
        )
    }

    pub fn sha256() -> Self {
        Self::build(true, ByteLimits::default(), ObjectFormat::SHA256)
    }

    pub fn warming() -> Self {
        Self::build(false, ByteLimits::default(), ObjectFormat::SHA1)
    }

    pub fn with_response_limit(response: ResponseLimit) -> Self {
        Self::build(
            true,
            ByteLimits {
                response,
                ..ByteLimits::default()
            },
            ObjectFormat::SHA1,
        )
    }

    pub fn with_archive_limit(archive: ArchiveLimit) -> Self {
        Self::build(
            true,
            ByteLimits {
                archive,
                ..ByteLimits::default()
            },
            ObjectFormat::SHA1,
        )
    }

    fn build(rebuilt: bool, byte_limits: ByteLimits, object_format: ObjectFormat) -> Self {
        Self::build_with_limits(rebuilt, byte_limits, object_format, LimitConfig::default())
    }

    fn build_with_limits(
        rebuilt: bool,
        byte_limits: ByteLimits,
        object_format: ObjectFormat,
        limits: LimitConfig,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let scan_path = dir.path().join("repos");
        std::fs::create_dir_all(&scan_path).unwrap();
        let knot = KnotId::new(format!("did:web:{KNOT_HOST}")).unwrap();
        let layout = Layout::new(&scan_path)
            .with_object_format(object_format)
            .reserving_meta(&knot)
            .unwrap();
        layout.bootstrap_meta(&knot).unwrap();
        let meta_path = layout.meta_path(&knot).unwrap();
        let index = Arc::new(Index::new(meta_path.clone(), layout.clone()));
        if rebuilt {
            index.rebuild().unwrap();
        }

        let responder: Responder = Box::new(|request| {
            let host = request.url.host_str().unwrap_or_default();
            let did = match host {
                "plc.directory" => request.url.path().trim_start_matches('/').to_string(),
                host if request.url.path().ends_with("/.well-known/did.json") => {
                    format!("did:web:{host}")
                }
                _ => String::new(),
            };
            let body = match did.starts_with("did:") {
                true => did_doc_for(&did),
                false => Bytes::new(),
            };
            Ok(HttpResponse {
                status: StatusCode::OK,
                headers: http::HeaderMap::new(),
                body,
            })
        });
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

        let lfs_store = dir.path().join("lfs");
        std::fs::create_dir_all(&lfs_store).unwrap();
        let lfs_handle = knot_lfs::LfsHandle::open(
            knot_lfs::LfsStorePath::new(&lfs_store),
            knot_lfs::LfsSize::new(64 * 1024 * 1024),
            knot_lfs::FreeSpaceFloor::new(0),
        )
        .unwrap();

        let state = Arc::new(XrpcState {
            layout: layout.clone(),
            index,
            atproto,
            secrets,
            entropy: Arc::new(OsEntropy),
            ci_logs: None,
            admins: BTreeSet::new(),
            admission: knot_types::AdmissionPolicy::Closed,
            knot_did: knot,
            knot_hostname: KnotHostname::new(KNOT_HOST).unwrap(),
            meta_path,
            knot_service_url: knot_types::KnotServiceUrl::new(format!("https://{KNOT_HOST}"))
                .unwrap(),
            limiter: Arc::new(PreAuthLimiter::with_config(limits)),
            cob_locks: Arc::new(CobLocks::default()),
            reservations: Arc::new(Reservations::new(
                knot_xrpc::ReservationTtl::new(1_000_000),
                PerActorQuota::new(16),
                GlobalQuota::new(16),
            )),
            proxy_trust: knot_types::ProxyTrust::default(),
            committer: knot_xrpc::Committer {
                name: AuthorName::new("Tangled"),
                email: Email::new("noreply@tangled.sh"),
            },
            byte_limits,
            budgets: Budgets::default(),
            git_http: Arc::new(FakeHttp::new(|_request: &HttpRequest| {
                Err(NetworkError::Connect(
                    "no git upstream is served in this test".to_string(),
                ))
            })),
            pack_limits: knot_pack::PackLimits::default(),
            service_owner: AccountDid::new(OWNER).unwrap(),
            events: Arc::new(knot_events::EventLog::new(
                ManualClock::new(UnixMicros::new(1_000_000_000)),
                knot_events::ReplayBounds::new(
                    knot_events::ReplayEvents::new(1024).unwrap(),
                    knot_events::ReplayBytes::new(16 << 20).unwrap(),
                ),
            )),
            subscriber_gate: Arc::new(knot_events::SubscriberGate::new(
                knot_events::GlobalSubscriberLimit::new(16),
                knot_events::PerPeerSubscriberLimit::new(4),
            )),
            maintenance: knot_maintenance::MaintenanceHandle::disabled(),
            appview: knot_types::AppviewEndpoint::new("https://tangled.test").unwrap(),
            slots: knot_resource::Slots::testing(8),
            lfs: Some(knot_xrpc::LfsWeb::new(lfs_handle, 8)),
            catalog: Arc::new(knot_messages::Catalog::defaults()),
        });
        let router = knot_xrpc::router(Arc::clone(&state));
        Self {
            _dir: dir,
            layout,
            lfs_dir: lfs_store,
            router,
            state,
        }
    }

    pub fn register(&self, did: &RepoDid, rkey: &str) {
        let meta = Repo::open(&self.state.meta_path).unwrap();
        let store = CobStore::new(&meta);
        let home = CobHome::from(&self.state.knot_did);
        let signer = self.state.secrets.signer(&self.state.knot_did).unwrap();
        let registration = Registration {
            owner: OwnerDid::new(OWNER).unwrap(),
            rkey: RepoRkey::new(rkey).unwrap(),
            name: RepoName::new(rkey).unwrap(),
            repo: did.clone(),
            created_at: UnixSeconds::new(1_000),
        };
        match store.list::<RepoRegistryCob>().unwrap().as_slice() {
            [] => {
                store
                    .create(
                        &home,
                        &RegistryChange::Register(registration),
                        &signer,
                        UnixSeconds::new(1_000),
                    )
                    .unwrap();
            }
            [object] => {
                register_repo(
                    &store,
                    &home,
                    *object,
                    registration,
                    &signer,
                    UnixSeconds::new(1_000),
                )
                .unwrap();
            }
            many => panic!("{} registry objects", many.len()),
        }
        self.state.index.refresh_registry().unwrap();
    }

    pub fn add_member(&self, subject: &str, added_by: &str, at: i64) {
        let meta = Repo::open(&self.state.meta_path).unwrap();
        let store = CobStore::new(&meta);
        let home = CobHome::from(&self.state.knot_did);
        let signer = self.state.secrets.signer(&self.state.knot_did).unwrap();
        let change = MembersChange::Add(grant(subject, added_by, at));
        match store.list::<MembersCob>().unwrap().as_slice() {
            [] => {
                store
                    .create(&home, &change, &signer, UnixSeconds::new(at))
                    .unwrap();
            }
            [object] => {
                store
                    .update(&home, *object, &change, &signer, UnixSeconds::new(at))
                    .unwrap();
            }
            many => panic!("{} members objects", many.len()),
        }
        self.state.index.refresh_members().unwrap();
    }

    pub fn add_collaborator(&self, repo: &RepoDid, subject: &str, added_by: &str, at: i64) {
        let git = self.layout.open(repo).unwrap();
        let store = CobStore::new(&git);
        let home = CobHome::from(repo);
        let signer = self.state.secrets.signer(&self.state.knot_did).unwrap();
        let change = CollaboratorsChange::Add(grant(subject, added_by, at));
        match store.list::<CollaboratorsCob>().unwrap().as_slice() {
            [] => {
                store
                    .create(&home, &change, &signer, UnixSeconds::new(at))
                    .unwrap();
            }
            [object] => {
                store
                    .update(&home, *object, &change, &signer, UnixSeconds::new(at))
                    .unwrap();
            }
            many => panic!("{} collaborators objects", many.len()),
        }
        self.state.index.refresh_collaborators(repo).unwrap();
    }
}

fn grant(subject: &str, added_by: &str, at: i64) -> Grant {
    Grant {
        subject: AccountDid::new(subject).unwrap(),
        added_by: AccountDid::new(added_by).unwrap(),
        created_at: UnixSeconds::new(at),
    }
}

fn actor_key() -> SigningKey {
    SigningKey::from_bytes(&[9u8; 32].into()).unwrap()
}

fn did_doc_for(did: &str) -> Bytes {
    let sec1 = actor_key()
        .verifying_key()
        .to_encoded_point(true)
        .as_bytes()
        .to_vec();
    let multikey = knot_types::crypto::multikey(0xe7, &sec1);
    let body = serde_json::json!({
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
            "serviceEndpoint": "https://pds.oyster.cafe"
        }]
    });
    Bytes::from(serde_json::to_vec(&body).unwrap())
}

static JTI: AtomicU64 = AtomicU64::new(0);

fn service_jwt(nsid: &str, actor: &str) -> String {
    let nonce = JTI.fetch_add(1, Ordering::SeqCst);
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"ES256K","typ":"JWT"}"#);
    let claims = serde_json::json!({
        "iss": actor,
        "aud": format!("did:web:{KNOT_HOST}"),
        "exp": 1_001,
        "iat": 999,
        "jti": format!("nonce-{nonce}"),
        "lxm": nsid,
    });
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    let signing_input = format!("{header}.{payload}");
    let signature: Signature = actor_key().sign(signing_input.as_bytes());
    format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    )
}

pub async fn post_authed(
    world: &World,
    path: &str,
    actor: &str,
    value: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let nsid = path
        .strip_prefix("/xrpc/")
        .expect("post_authed path names an xrpc method");
    let token = service_jwt(nsid, actor);
    let request = http::Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&value).unwrap()))
        .unwrap();
    let response = world.router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

pub async fn post_json(
    world: &World,
    path: &str,
    value: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let request = http::Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&value).unwrap()))
        .unwrap();
    let response = world.router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

pub fn git_run(cwd: &Path, when: &str, author: (&str, &str), args: &[&str]) -> String {
    let output = knot_fixtures::command_at(cwd, when)
        .args(args)
        .env("GIT_AUTHOR_NAME", author.0)
        .env("GIT_AUTHOR_EMAIL", author.1)
        .env("GIT_COMMITTER_NAME", author.0)
        .env("GIT_COMMITTER_EMAIL", author.1)
        .output()
        .expect("git is available");
    assert!(
        output.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

pub fn sh_git_at(cwd: &Path, when: &str, args: &[&str]) -> String {
    git_run(cwd, when, ("nel", "nel@oyster.cafe"), args)
}

pub fn sh_git(cwd: &Path, args: &[&str]) -> String {
    sh_git_at(cwd, "2026-06-01T12:30:00+02:00", args)
}

pub fn commit_file(work: &Path, file: &str, contents: &[u8], message: &str, when: &str) {
    let target = work.join(file);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(target, contents).unwrap();
    sh_git_at(work, when, &["add", "-A"]);
    sh_git_at(work, when, &["commit", "-q", "-m", message]);
}

pub fn seeded(world: &World, rkey: &str) -> (RepoDid, tempfile::TempDir) {
    seeded_with_format(world, rkey, ObjectFormat::SHA1)
}

pub fn seeded_with_format(
    world: &World,
    rkey: &str,
    object_format: ObjectFormat,
) -> (RepoDid, tempfile::TempDir) {
    let did = RepoDid::new(format!("did:plc:{rkey}fixture")).unwrap();
    world.layout.create(&did).unwrap();
    world.register(&did, rkey);
    let bare = world.layout.repo_path(&did).unwrap();
    let work_dir = tempfile::tempdir().unwrap();
    let work = work_dir.path();
    let init = match object_format == ObjectFormat::SHA256 {
        true => vec!["init", "-q", "--object-format=sha256", "-b", "main"],
        false => vec!["init", "-q", "-b", "main"],
    };
    sh_git(work, &init);
    commit_file(
        work,
        "README.md",
        b"# coral\n\nhello\n",
        "first",
        "2026-06-01T12:30:00+02:00",
    );
    commit_file(
        work,
        "src/main.rs",
        b"fn main() {\n    println!(\"reef\");\n}\n",
        "add main",
        "2026-06-01T12:31:00+02:00",
    );
    commit_file(
        work,
        "logo.png",
        b"\x89PNG\r\n\x1a\n0000binarybytes\x00\x01",
        "add logo",
        "2026-06-01T12:32:00+02:00",
    );
    sh_git(work, &["tag", "lightweight"]);
    sh_git_at(
        work,
        "2026-06-01T12:32:30+02:00",
        &["tag", "-a", "v1.0.0", "-m", "release one"],
    );
    commit_file(
        work,
        "README.md",
        b"# coral\n\nhello reef\n",
        "update readme",
        "2026-06-01T12:33:00+02:00",
    );
    sh_git(
        work,
        &["push", "-q", "--tags", bare.to_str().unwrap(), "main"],
    );
    (did, work_dir)
}

pub fn empty_repo(world: &World, rkey: &str) -> (RepoDid, String, tempfile::TempDir) {
    let did = RepoDid::new(format!("did:plc:{rkey}fixture")).unwrap();
    world.layout.create(&did).unwrap();
    world.register(&did, rkey);
    let bare = world
        .layout
        .repo_path(&did)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let work_dir = tempfile::tempdir().unwrap();
    sh_git(work_dir.path(), &["init", "-q", "-b", "main"]);
    (did, bare, work_dir)
}

pub fn seeded_feature_branch(world: &World, rkey: &str) -> (RepoDid, Oid, Oid) {
    let (did, bare, work_dir) = empty_repo(world, rkey);
    let work = work_dir.path();
    commit_file(
        work,
        "reef.txt",
        b"one\ntwo\n",
        "base",
        "2026-06-01T12:30:00+02:00",
    );
    sh_git(work, &["checkout", "-q", "-b", "feature"]);
    commit_file(
        work,
        "reef.txt",
        b"one\nTWO\n",
        "capitalize two\n\nbecause waves",
        "2026-06-01T12:31:00+02:00",
    );
    commit_file(
        work,
        "kelp.txt",
        b"frond\n",
        "add kelp",
        "2026-06-01T12:32:00+02:00",
    );
    sh_git(work, &["push", "-q", &bare, "main", "feature"]);
    let main = Oid::from_hex(&sh_git(work, &["rev-parse", "main"])).unwrap();
    let feature = Oid::from_hex(&sh_git(work, &["rev-parse", "feature"])).unwrap();
    (did, main, feature)
}

pub async fn get_with_headers(
    world: &World,
    path_and_query: &str,
    headers: HeaderMap,
) -> (StatusCode, HeaderMap, Bytes) {
    let mut request = http::Request::builder()
        .method("GET")
        .uri(path_and_query)
        .body(Body::empty())
        .unwrap();
    request.headers_mut().extend(headers);
    let response = world.router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let response_headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, response_headers, body)
}

pub async fn get(world: &World, path_and_query: &str) -> (StatusCode, HeaderMap, Bytes) {
    get_with_headers(world, path_and_query, HeaderMap::new()).await
}

pub async fn get_json(world: &World, path_and_query: &str) -> serde_json::Value {
    let (status, _, body) = get(world, path_and_query).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "GET {path_and_query} failed: {}",
        String::from_utf8_lossy(&body)
    );
    serde_json::from_slice(&body).unwrap()
}

pub async fn get_error(world: &World, path_and_query: &str) -> (StatusCode, String) {
    let (status, _, body) = get(world, path_and_query).await;
    assert!(!status.is_success(), "GET {path_and_query} unexpectedly ok");
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    (status, value["error"].as_str().unwrap().to_string())
}

pub fn ref_names(value: &serde_json::Value, key: &str) -> Vec<String> {
    value[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["ref"].as_str().unwrap().to_string())
        .collect()
}

pub fn repo_dids(value: &serde_json::Value) -> Vec<String> {
    value["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["repo"].as_str().unwrap().to_string())
        .collect()
}

pub async fn archive_full(world: &World, did: &RepoDid) -> (String, String, Bytes) {
    let (status, headers, body) = get(
        world,
        &format!("/xrpc/sh.tangled.repo.archive?repo={did}&ref=main"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let etag = headers
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let last_modified = headers
        .get(header::LAST_MODIFIED)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    (etag, last_modified, body)
}

pub async fn assert_immutable_round_trip(
    world: &World,
    headers: &HeaderMap,
    full: &Bytes,
    etag: &str,
) {
    let link = headers.get(header::LINK).unwrap().to_str().unwrap();
    let immutable = link
        .trim_start_matches('<')
        .split('>')
        .next()
        .unwrap()
        .strip_prefix(&format!("https://{KNOT_HOST}"))
        .expect("the immutable link points at this knot");
    let (status, immutable_headers, immutable_body) = get(world, immutable).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        &immutable_body, full,
        "following the immutable link regenerates the very bytes it was attached to"
    );
    assert_eq!(
        immutable_headers
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap(),
        etag,
        "the immutable link shares the etag of the response that advertised it"
    );
}

pub async fn assert_warming(world: &World, path: &str, expected_error: Option<&str>) {
    let (status, error) = get_error(world, path).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "path {path}");
    if let Some(expected) = expected_error {
        assert_eq!(error, expected, "path {path}");
    }
}

pub async fn assert_post_rejected(
    world: &World,
    path: &str,
    actor: &str,
    value: serde_json::Value,
) {
    let (status, body) = post_authed(world, path, actor, value).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
    assert_eq!(body["error"], "InvalidRequest", "{path}");
}
