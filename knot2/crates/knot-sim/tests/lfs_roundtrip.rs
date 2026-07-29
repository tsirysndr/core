mod common;

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use http::Method;
use knot_atproto::Atproto;
use knot_cob::{CobHome, CobStore};
use knot_cobs::{Registration, RegistryChange};
use knot_edge::RequiresFullHandshake;
use knot_git::{Layout, Repo};
use knot_lfs::{FreeSpaceFloor, LfsHandle, LfsOid, LfsSize, LfsStore, LfsStorePath};
use knot_runtime::{
    FakeHttp, HttpResponse, K256Signer, ManualClock, OsEntropy, Signer, UnixMicros,
};
use knot_secrets::{MasterKey, SealedStore};
use knot_types::{
    AccountDid, AdmissionPolicy, AuthorName, Email, KnotHostname, KnotId, OwnerDid, RepoDid,
    RepoName, RepoRkey, UnixSeconds,
};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tower::ServiceExt;
use url::Url;

const REPO_DID: &str = "did:plc:squid";
const REPO_NAME: &str = "anemone";
const FORK_NAME: &str = "anemone-fork";
const OWNER_DID: &str = "did:plc:nel";
const PDS_HOST: &str = "pds.oyster.cafe";
const KNOT_DID: &str = "did:web:nel.pet";
const PINNED_DATE: &str = "2026-07-07T12:00:00+00:00";

fn require_git_lfs() -> bool {
    let available = Command::new("git-lfs")
        .arg("version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    match (available, std::env::var("KNOT_LFS_ROUNDTRIP").as_deref()) {
        (true, _) => true,
        (false, Ok("skip")) => {
            eprintln!(
                "skipping lfs round trip gate: git-lfs unavailable and KNOT_LFS_ROUNDTRIP=skip"
            );
            false
        }
        (false, _) => panic!(
            "the lfs round trip gate found no working git-lfs on PATH. \
             Install git-lfs or set KNOT_LFS_ROUNDTRIP=skip to skip the gate."
        ),
    }
}

fn media_bytes() -> Vec<u8> {
    (0..1_048_576u32)
        .map(|n| (n.wrapping_mul(31) % 251) as u8)
        .collect()
}

fn second_media_bytes() -> Vec<u8> {
    (0..524_288u32)
        .map(|n| (n.wrapping_mul(97).wrapping_add(13) % 253) as u8)
        .collect()
}

fn require_scutiger() -> bool {
    let available = Command::new("git-lfs-transfer")
        .arg("--help")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    match (available, std::env::var("KNOT_LFS_CONFORMANCE").as_deref()) {
        (true, _) => true,
        (false, Ok("skip")) => {
            eprintln!(
                "skipping lfs conformance gate: git-lfs-transfer unavailable and \
                 KNOT_LFS_CONFORMANCE=skip"
            );
            false
        }
        (false, _) => panic!(
            "the lfs conformance gate found no scutiger git-lfs-transfer on PATH. \
             Install it or set KNOT_LFS_CONFORMANCE=skip to skip the gate."
        ),
    }
}

fn git(cwd: &Path, env: &[(String, String)], args: &[&str]) -> (bool, String) {
    let mut command = knot_fixtures::command_at(cwd, PINNED_DATE);
    command.args(args);
    env.iter().for_each(|(key, value)| {
        command.env(key, value);
    });
    let out = command.output().expect("git runs");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

fn keygen(dir: &Path) -> (String, String) {
    let path = dir.join("client");
    let out = Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "nel@oyster.cafe",
            "-f",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("ssh-keygen runs");
    assert!(out.status.success());
    let public_line = std::fs::read_to_string(dir.join("client.pub"))
        .unwrap()
        .trim()
        .to_string();
    (path.to_str().unwrap().to_string(), public_line)
}

fn actor_signer() -> K256Signer {
    K256Signer::from_slice(&[9u8; 32]).unwrap()
}

fn did_document(did: &str) -> Vec<u8> {
    let multikey = knot_types::crypto::multikey(0xe7, actor_signer().public_key().as_bytes());
    serde_json::to_vec(&serde_json::json!({
        "id": did,
        "alsoKnownAs": ["at://nel.pet"],
        "verificationMethod": [{
            "id": format!("{did}#atproto"),
            "type": "Multikey",
            "controller": did,
            "publicKeyMultibase": multikey
        }],
        "service": [{
            "id": "#atproto_pds",
            "type": "AtprotoPersonalDataServer",
            "serviceEndpoint": format!("https://{PDS_HOST}")
        }]
    }))
    .unwrap()
}

fn list_records_body(public_line: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "records": [{
            "uri": format!("at://{OWNER_DID}/sh.tangled.publicKey/1"),
            "value": {
                "$type": "sh.tangled.publicKey",
                "key": public_line,
                "name": "laptop",
                "createdAt": "2026-07-01T00:00:00Z"
            }
        }]
    }))
    .unwrap()
}

fn fake_http(
    published_line: String,
) -> FakeHttp<
    impl Fn(&knot_runtime::HttpRequest) -> Result<HttpResponse, knot_runtime::NetworkError>
    + Send
    + Sync,
> {
    FakeHttp::new(move |request: &knot_runtime::HttpRequest| {
        let host = request.url.host_str().unwrap_or_default().to_string();
        let path = request.url.path().to_string();
        let body = if host == PDS_HOST {
            list_records_body(&published_line)
        } else if host == "plc.directory" && request.method == http::Method::POST {
            b"{}".to_vec()
        } else if host == "plc.directory" && path.starts_with("/did:") {
            did_document(path.trim_start_matches('/'))
        } else {
            return Ok(HttpResponse {
                status: http::StatusCode::NOT_FOUND,
                headers: http::HeaderMap::new(),
                body: bytes::Bytes::new(),
            });
        };
        Ok(HttpResponse {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body: bytes::Bytes::from(body),
        })
    })
}

fn service_jwt(nsid: &str, jti: &str) -> String {
    service_jwt_as(OWNER_DID, nsid, jti)
}

fn service_jwt_as(iss: &str, nsid: &str, jti: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"ES256K","typ":"JWT"}"#);
    let claims = serde_json::json!({
        "iss": iss,
        "aud": KNOT_DID,
        "exp": 1_001,
        "iat": 999,
        "jti": jti,
        "lxm": nsid,
    });
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    let signing_input = format!("{header}.{payload}");
    let signature = actor_signer().sign(signing_input.as_bytes());
    format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.as_bytes())
    )
}

struct World {
    _scan: TempDir,
    lfs: LfsHandle,
    ssh_port: u16,
    http_base: String,
    router: axum::Router,
    layout: Layout,
    h3: Option<common::Edge>,
    _certdir: Option<TempDir>,
}

async fn spawn_world(published_line: String) -> World {
    spawn(published_line, false).await
}

async fn spawn(published_line: String, with_h3: bool) -> World {
    let scan = tempfile::tempdir().unwrap();
    let meta_path = scan.path().join("meta");
    Repo::create(&meta_path).unwrap();
    let layout = Layout::new(scan.path().join("repos"));
    let repo_did = RepoDid::new(REPO_DID).unwrap();
    layout.create(&repo_did).unwrap();

    let knot = KnotId::new(KNOT_DID).unwrap();
    let secrets = Arc::new(
        SealedStore::open(
            scan.path().join("keys.sealed"),
            &MasterKey::new([7u8; 32]).unwrap(),
            Box::new(OsEntropy),
        )
        .unwrap(),
    );
    secrets.ensure(&knot).unwrap();
    let knot_signer = secrets.signer(&knot).unwrap();

    let meta = Repo::open(&meta_path).unwrap();
    CobStore::new(&meta)
        .create(
            &CobHome::from(&knot),
            &RegistryChange::Register(Registration {
                owner: OwnerDid::new(OWNER_DID).unwrap(),
                rkey: RepoRkey::new(REPO_NAME).unwrap(),
                name: RepoName::new(REPO_NAME).unwrap(),
                repo: repo_did.clone(),
                created_at: UnixSeconds::new(1),
            }),
            &knot_signer,
            UnixSeconds::new(1),
        )
        .unwrap();

    let index = Arc::new(knot_index::Index::new(meta_path.clone(), layout.clone()));
    index.rebuild().unwrap();

    let atproto = Arc::new(Atproto::new(
        fake_http(published_line),
        ManualClock::new(UnixMicros::new(1_000_000_000)),
        knot.clone(),
        knot_atproto::PlcDirectory::new(Url::parse("https://plc.directory/").unwrap()).unwrap(),
    ));

    let lfs_store_dir = scan.path().join("lfs");
    std::fs::create_dir_all(&lfs_store_dir).unwrap();
    let lfs = LfsHandle::open(
        LfsStorePath::new(&lfs_store_dir),
        LfsSize::new(64 * 1024 * 1024),
        FreeSpaceFloor::new(0),
    )
    .unwrap();

    let key_dir = scan.path().join("hostkey");
    std::fs::create_dir_all(&key_dir).unwrap();
    let host_key = knot_ssh::load_or_create_host_key(&key_dir.join("host")).unwrap();
    let events = Arc::new(knot_events::EventLog::new(
        ManualClock::new(UnixMicros::new(1_000_000_000)),
        knot_events::ReplayBounds::new(
            knot_events::ReplayEvents::new(64).unwrap(),
            knot_events::ReplayBytes::new(16 << 20).unwrap(),
        ),
    ));
    let ssh_state = Arc::new(
        knot_ssh::SshState::new(knot_ssh::SshConfig {
            layout: layout.clone(),
            index: Arc::clone(&index),
            atproto: Arc::clone(&atproto),
            knot_actor: knot_types::ActorId::from_secp256k1(actor_signer().public_key().as_bytes()),
            events: Arc::clone(&events),
            hostname: KnotHostname::new("nel.pet").unwrap(),
            appview: knot_types::AppviewEndpoint::new("https://tangled.test").unwrap(),
            admins: BTreeSet::from([AccountDid::new(OWNER_DID).unwrap()]),
            admission: AdmissionPolicy::Closed,
            max_pack_bytes: knot_xrpc::MaxWireBytes::new(1 << 30),
            archive_limit: knot_git::ArchiveLimit::default(),
            languages_push_budget: knot_xrpc::LanguagesPushBudget::new(Duration::from_secs(2)),
            ci_logs: None,
        })
        .with_lfs(lfs.clone(), 16),
    );
    let ssh_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ssh_port = ssh_listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = knot_ssh::serve_on_socket(ssh_listener, host_key, ssh_state).await;
    });

    let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_base = format!(
        "http://127.0.0.1:{}",
        http_listener.local_addr().unwrap().port()
    );

    let xrpc_state = Arc::new(knot_xrpc::XrpcState {
        ci_logs: None,
        layout: layout.clone(),
        index: Arc::clone(&index),
        atproto,
        secrets,
        entropy: Arc::new(OsEntropy),
        admins: BTreeSet::from([AccountDid::new(OWNER_DID).unwrap()]),
        admission: AdmissionPolicy::Closed,
        knot_did: knot,
        knot_hostname: KnotHostname::new("nel.pet").unwrap(),
        meta_path,
        knot_service_url: knot_types::KnotServiceUrl::new(http_base.clone()).unwrap(),
        limiter: Arc::new(knot_xrpc::PreAuthLimiter::default()),
        cob_locks: Arc::new(knot_xrpc::CobLocks::default()),
        reservations: Arc::new(knot_xrpc::Reservations::new(
            knot_xrpc::ReservationTtl::new(1_000_000),
            knot_xrpc::PerActorQuota::new(16),
            knot_xrpc::GlobalQuota::new(16),
        )),
        proxy_trust: knot_types::ProxyTrust::default(),
        committer: knot_xrpc::Committer {
            name: AuthorName::new("Tangled"),
            email: Email::new("noreply@tangled.sh"),
        },
        byte_limits: knot_xrpc::ByteLimits {
            pack: knot_xrpc::MaxWireBytes::new(1 << 30),
            ..knot_xrpc::ByteLimits::default()
        },
        budgets: knot_xrpc::Budgets::default(),
        git_http: Arc::new(FakeHttp::new(|_request: &knot_runtime::HttpRequest| {
            Err(knot_runtime::NetworkError::Connect(
                "no remote upstream is served in this gate".to_string(),
            ))
        })),
        pack_limits: knot_pack::PackLimits::default(),
        service_owner: AccountDid::new(OWNER_DID).unwrap(),
        events,
        subscriber_gate: Arc::new(knot_events::SubscriberGate::new(
            knot_events::GlobalSubscriberLimit::new(16),
            knot_events::PerPeerSubscriberLimit::new(4),
        )),
        maintenance: knot_maintenance::MaintenanceHandle::disabled(),
        appview: knot_types::AppviewEndpoint::new("https://tangled.test").unwrap(),
        slots: knot_resource::Slots::testing(8),
        lfs: Some(knot_xrpc::LfsWeb::new(lfs.clone(), 8)),
        catalog: Arc::new(knot_messages::Catalog::defaults()),
    });

    let resolver: Arc<dyn knot_pack::RepoResolver> = {
        let index = Arc::clone(&index);
        Arc::new(move |target: &knot_pack::RepoTarget| match target {
            knot_pack::RepoTarget::Did(did) => match index.owner_of(did) {
                knot_index::Resolved::Ready(Some(_)) => knot_pack::RepoLookup::Hosted(did.clone()),
                knot_index::Resolved::Ready(None) => knot_pack::RepoLookup::Unhosted,
                knot_index::Resolved::Warming => knot_pack::RepoLookup::Unavailable,
            },
            knot_pack::RepoTarget::OwnerPath(owner, path) => {
                match index.resolve_clone_path(owner, path) {
                    knot_index::Resolved::Ready(Some(found)) => {
                        knot_pack::RepoLookup::Hosted(found)
                    }
                    knot_index::Resolved::Ready(None) => knot_pack::RepoLookup::Unhosted,
                    knot_index::Resolved::Warming => knot_pack::RepoLookup::Unavailable,
                }
            }
        })
    };
    let advertiser = knot_xrpc::receive_advertiser(Arc::clone(&xrpc_state));
    let (write_routes, advertisement) = knot_pack::edge_routes(knot_pack::EdgeConfig {
        receive: Some(Arc::clone(&advertiser)),
        pack_slots: knot_resource::PackSlots::new(4),
        ..knot_pack::EdgeConfig::serving(
            layout.clone(),
            Arc::clone(&resolver),
            Arc::new(knot_runtime::SystemClock),
        )
    });
    let router = write_routes
        .merge(advertisement.into_router())
        .merge(knot_xrpc::router(Arc::clone(&xrpc_state)));
    let served = router.clone();
    tokio::spawn(async move {
        let _ = axum::serve(http_listener, served).await;
    });

    let (h3, certdir) = match with_h3 {
        true => {
            let certdir = tempfile::tempdir().unwrap();
            let edge = common::serve_edge(certdir.path(), || {
                let (write_routes, advertisement) = knot_pack::edge_routes(knot_pack::EdgeConfig {
                    receive: Some(Arc::clone(&advertiser)),
                    pack_slots: knot_resource::PackSlots::new(4),
                    ..knot_pack::EdgeConfig::serving(
                        layout.clone(),
                        Arc::clone(&resolver),
                        Arc::new(knot_runtime::SystemClock),
                    )
                });
                let app = RequiresFullHandshake::new(
                    write_routes.merge(knot_xrpc::router(Arc::clone(&xrpc_state))),
                );
                (app, advertisement)
            })
            .await;
            (Some(edge), Some(certdir))
        }
        false => (None, None),
    };

    World {
        _scan: scan,
        lfs,
        ssh_port,
        http_base,
        router,
        layout,
        h3,
        _certdir: certdir,
    }
}

async fn in_git_blocking<T: Send + 'static>(task: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(task).await.unwrap()
}

fn seed_lfs_work(work: &Path, env: &[(String, String)], media: &[u8]) {
    std::fs::create_dir_all(work).unwrap();
    let steps: [&[&str]; 2] = [
        &["init", "-q", "-b", "main"],
        &["lfs", "install", "--local"],
    ];
    steps.iter().for_each(|args| {
        let (ok, out) = git(work, env, args);
        assert!(ok, "{args:?} failed:\n{out}");
    });
    let (ok, out) = git(work, env, &["lfs", "track", "*.bin"]);
    assert!(ok, "lfs track failed:\n{out}");
    let (ok, out) = git(work, env, &["config", "lfs.locksverify", "false"]);
    assert!(ok, "config failed:\n{out}");
    std::fs::write(work.join("media.bin"), media).unwrap();
    std::fs::write(work.join("README.md"), "media lives in lfs\n").unwrap();
    let commit: [&[&str]; 2] = [&["add", "-A"], &["commit", "-q", "-m", "media"]];
    commit.iter().for_each(|args| {
        let (ok, out) = git(work, env, args);
        assert!(ok, "{args:?} failed:\n{out}");
    });
}

fn clone_and_pull(base: &Path, url: &str, name: &str, env: &[(String, String)]) -> PathBuf {
    let skip_smudge: Vec<(String, String)> = env
        .iter()
        .cloned()
        .chain([("GIT_LFS_SKIP_SMUDGE".to_string(), "1".to_string())])
        .collect();
    let (ok, out) = git(base, &skip_smudge, &["clone", "-q", url, name]);
    assert!(ok, "anonymous clone of {url} failed:\n{out}");
    let dst = base.join(name);
    let pointer = std::fs::read_to_string(dst.join("media.bin")).unwrap();
    assert!(
        pointer.contains("git-lfs.github.com/spec/v1"),
        "clone must land the pointer before lfs pull, got:\n{pointer}"
    );
    let (ok, out) = git(&dst, env, &["lfs", "install", "--local"]);
    assert!(ok, "lfs install in {name} failed:\n{out}");
    let (ok, out) = git(&dst, env, &["lfs", "pull"]);
    assert!(ok, "git lfs pull in {name} failed:\n{out}");
    dst
}

async fn create_fork(world: &World, jti: &str) -> (http::StatusCode, serde_json::Value) {
    let token = service_jwt("sh.tangled.repo.create", jti);
    let body = serde_json::json!({
        "rkey": FORK_NAME,
        "name": FORK_NAME,
        "source": format!("{}/{OWNER_DID}/{REPO_NAME}", world.http_base),
    });
    let request = http::Request::builder()
        .method("POST")
        .uri("/xrpc/sh.tangled.repo.create")
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = world.router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_lfs_round_trip_gate_holds_over_both_transports_and_the_fork() {
    if !require_git_lfs() {
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let (key_path, public_line) = keygen(scratch.path());
    let world = spawn_world(public_line).await;

    let media = media_bytes();
    let media_oid = LfsOid::from_digest(Sha256::digest(&media).into());
    let ssh = format!(
        "ssh -i {key_path} -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
         -o UserKnownHostsFile=/dev/null -o PreferredAuthentications=publickey -o BatchMode=yes"
    );
    let path_env = std::env::var("PATH").unwrap_or_default();
    let home = scratch.path().to_str().unwrap().to_string();
    let env: Vec<(String, String)> = [
        ("GIT_SSH_COMMAND", &ssh),
        ("PATH", &path_env),
        ("HOME", &home),
    ]
    .map(|(key, value)| (key.to_string(), value.clone()))
    .to_vec();

    let work = scratch.path().join("work");
    seed_lfs_work(&work, &env, &media);

    let push_url = format!(
        "ssh://git@127.0.0.1:{}/{OWNER_DID}/{REPO_NAME}",
        world.ssh_port
    );
    let (ok, out) = {
        let work = work.clone();
        let env = env.clone();
        in_git_blocking(move || git(&work, &env, &["push", "-q", &push_url, "main"])).await
    };
    assert!(ok, "lfs push over ssh failed:\n{out}");

    let source_repo = RepoDid::new(REPO_DID).unwrap();
    assert_eq!(
        world.lfs.store.probe(&source_repo, &media_oid).unwrap(),
        Some(LfsSize::new(media.len() as u64)),
        "pushed media must be durable in the store"
    );

    let clone_url = format!("{}/{OWNER_DID}/{REPO_NAME}", world.http_base);
    let dst = {
        let base = scratch.path().to_path_buf();
        let env = env.clone();
        in_git_blocking(move || clone_and_pull(&base, &clone_url, "reader", &env)).await
    };
    assert_eq!(
        std::fs::read(dst.join("media.bin")).unwrap(),
        media,
        "anonymous http reader must see byte-identical media"
    );

    let (status, created) = create_fork(&world, "gate-fork-1").await;
    assert_eq!(
        status,
        http::StatusCode::OK,
        "fork create failed: {created}"
    );
    assert!(
        created.get("lfsMissing").is_none(),
        "local fork must copy every object, got {created}"
    );
    let fork_did = RepoDid::new(created["repoDid"].as_str().unwrap()).unwrap();
    assert_eq!(
        world.lfs.store.probe(&fork_did, &media_oid).unwrap(),
        Some(LfsSize::new(media.len() as u64)),
        "fork prefix must hold its own copy of the media"
    );

    let fork_url = format!("{}/{OWNER_DID}/{FORK_NAME}", world.http_base);
    let fork_dst = {
        let base = scratch.path().to_path_buf();
        let env = env.clone();
        in_git_blocking(move || clone_and_pull(&base, &fork_url, "fork-reader", &env)).await
    };
    assert_eq!(
        std::fs::read(fork_dst.join("media.bin")).unwrap(),
        media,
        "anonymous clone of the fork must see byte-identical media"
    );
}

fn seed_many_lfs(work: &Path, env: &[(String, String)], count: usize) {
    std::fs::create_dir_all(work).unwrap();
    let steps: [&[&str]; 2] = [
        &["init", "-q", "-b", "main"],
        &["lfs", "install", "--local"],
    ];
    steps.iter().for_each(|args| {
        let (ok, out) = git(work, env, args);
        assert!(ok, "{args:?} failed:\n{out}");
    });
    let (ok, out) = git(work, env, &["lfs", "track", "*.bin"]);
    assert!(ok, "lfs track failed:\n{out}");
    let (ok, out) = git(work, env, &["config", "lfs.locksverify", "false"]);
    assert!(ok, "config failed:\n{out}");
    (0..count).for_each(|index| {
        let size = 200 + index * 7;
        let bytes: Vec<u8> = (0..size)
            .map(|n| (n.wrapping_mul(31).wrapping_add(index) % 251) as u8)
            .collect();
        std::fs::write(work.join(format!("object-{index}.bin")), bytes).unwrap();
    });
    let commit: [&[&str]; 2] = [&["add", "-A"], &["commit", "-q", "-m", "many media"]];
    commit.iter().for_each(|args| {
        let (ok, out) = git(work, env, args);
        assert!(ok, "{args:?} failed:\n{out}");
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_objects_ride_default_git_lfs_concurrency_over_both_transports() {
    if !require_git_lfs() {
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let (key_path, public_line) = keygen(scratch.path());
    let world = spawn_world(public_line).await;

    let ssh = format!(
        "ssh -i {key_path} -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
         -o UserKnownHostsFile=/dev/null -o PreferredAuthentications=publickey -o BatchMode=yes"
    );
    let path_env = std::env::var("PATH").unwrap_or_default();
    let home = scratch.path().to_str().unwrap().to_string();
    let env: Vec<(String, String)> = [
        ("GIT_SSH_COMMAND", &ssh),
        ("PATH", &path_env),
        ("HOME", &home),
    ]
    .map(|(key, value)| (key.to_string(), value.clone()))
    .to_vec();

    let count = 25usize;
    let work = scratch.path().join("work");
    seed_many_lfs(&work, &env, count);

    let push_url = format!(
        "ssh://git@127.0.0.1:{}/{OWNER_DID}/{REPO_NAME}",
        world.ssh_port
    );
    let (ok, out) = {
        let work = work.clone();
        let env = env.clone();
        in_git_blocking(move || git(&work, &env, &["push", "-q", &push_url, "main"])).await
    };
    assert!(
        ok,
        "git-lfs at its default concurrency must push {count} objects over ssh without tripping \
         the per-peer connection limit:\n{out}"
    );

    let clone_url = format!("{}/{OWNER_DID}/{REPO_NAME}", world.http_base);
    let dst = {
        let base = scratch.path().to_path_buf();
        let env = env.clone();
        in_git_blocking(move || {
            let skip_smudge: Vec<(String, String)> = env
                .iter()
                .cloned()
                .chain([("GIT_LFS_SKIP_SMUDGE".to_string(), "1".to_string())])
                .collect();
            let (ok, out) = git(&base, &skip_smudge, &["clone", "-q", &clone_url, "reader"]);
            assert!(ok, "anonymous clone failed:\n{out}");
            let dst = base.join("reader");
            let (ok, out) = git(&dst, &env, &["lfs", "install", "--local"]);
            assert!(ok, "lfs install failed:\n{out}");
            let (ok, out) = git(&dst, &env, &["lfs", "pull"]);
            assert!(
                ok,
                "anonymous http pull of {count} objects mustn't be throttled by the xrpc \
                 pre-auth limiter:\n{out}"
            );
            dst
        })
        .await
    };
    (0..count).for_each(|index| {
        assert_eq!(
            std::fs::read(work.join(format!("object-{index}.bin"))).unwrap(),
            std::fs::read(dst.join(format!("object-{index}.bin"))).unwrap(),
            "object-{index}.bin must be byte-identical over anonymous http"
        );
    });
}

fn write_shim(dir: &Path) -> String {
    let shim = dir.join("local-ssh.sh");
    std::fs::write(
        &shim,
        "#!/bin/sh\n\
         while [ \"$#\" -gt 0 ]; do\n\
           case \"$1\" in\n\
             -o|-p) shift 2 ;;\n\
             -*) shift ;;\n\
             *) break ;;\n\
           esac\n\
         done\n\
         shift\n\
         eval exec \"$@\"\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&shim).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&shim, permissions).unwrap();
    shim.to_str().unwrap().to_string()
}

fn hex_object_files(root: &Path) -> Vec<(String, u64, PathBuf)> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    entries
        .filter_map(Result::ok)
        .flat_map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                return hex_object_files(&path);
            }
            path.file_name()
                .and_then(|name| name.to_str())
                .filter(|name| LfsOid::new(*name).is_ok())
                .map(|name| {
                    let size = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
                    vec![(name.to_string(), size, path.clone())]
                })
                .unwrap_or_default()
        })
        .collect()
}

fn pull_verdict(base: &Path, url: &str, name: &str, env: &[(String, String)]) -> (bool, String) {
    let skip_smudge: Vec<(String, String)> = env
        .iter()
        .cloned()
        .chain([("GIT_LFS_SKIP_SMUDGE".to_string(), "1".to_string())])
        .collect();
    let (ok, out) = git(base, &skip_smudge, &["clone", "-q", url, name]);
    assert!(ok, "clone of {url} failed:\n{out}");
    let dst = base.join(name);
    let (ok, out) = git(&dst, env, &["lfs", "install", "--local"]);
    assert!(ok, "lfs install in {name} failed:\n{out}");
    git(&dst, env, &["lfs", "pull"])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_lfs_stack_is_conformant_with_the_reference_server_and_client() {
    if !require_git_lfs() || !require_scutiger() {
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let (key_path, public_line) = keygen(scratch.path());
    let world = spawn_world(public_line).await;

    let media = media_bytes();
    let second = second_media_bytes();
    let media_oid = LfsOid::from_digest(Sha256::digest(&media).into());
    let second_oid = LfsOid::from_digest(Sha256::digest(&second).into());
    let expected: std::collections::BTreeSet<(String, u64)> = [
        (media_oid.as_str().to_string(), media.len() as u64),
        (second_oid.as_str().to_string(), second.len() as u64),
    ]
    .into();

    let ssh = format!(
        "ssh -i {key_path} -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
         -o UserKnownHostsFile=/dev/null -o PreferredAuthentications=publickey -o BatchMode=yes"
    );
    let path_env = std::env::var("PATH").unwrap_or_default();
    let home = scratch.path().to_str().unwrap().to_string();
    let knot_env: Vec<(String, String)> = [
        ("GIT_SSH_COMMAND", &ssh),
        ("PATH", &path_env),
        ("HOME", &home),
    ]
    .map(|(key, value)| (key.to_string(), value.clone()))
    .to_vec();
    let shim = write_shim(scratch.path());
    let reference_env: Vec<(String, String)> = [
        ("GIT_SSH_COMMAND", &shim),
        ("PATH", &path_env),
        ("HOME", &home),
    ]
    .map(|(key, value)| (key.to_string(), value.clone()))
    .to_vec();

    let upstream = scratch.path().join("reference-upstream.git");
    let (ok, out) = git(
        scratch.path(),
        &reference_env,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            upstream.to_str().unwrap(),
        ],
    );
    assert!(ok, "reference upstream init failed:\n{out}");

    let work = scratch.path().join("work");
    seed_lfs_work(&work, &knot_env, &media);
    std::fs::write(work.join("extra.bin"), &second).unwrap();
    let commit: [&[&str]; 2] = [&["add", "-A"], &["commit", "-q", "-m", "extra media"]];
    commit.iter().for_each(|args| {
        let (ok, out) = git(&work, &knot_env, args);
        assert!(ok, "{args:?} failed:\n{out}");
    });

    let knot_push_url = format!(
        "ssh://git@127.0.0.1:{}/{OWNER_DID}/{REPO_NAME}",
        world.ssh_port
    );
    let reference_push_url = format!("ssh://ref@localhost{}", upstream.display());
    let pushes = {
        let work = work.clone();
        let knot_env = knot_env.clone();
        let reference_env = reference_env.clone();
        let knot_push_url = knot_push_url.clone();
        let reference_push_url = reference_push_url.clone();
        in_git_blocking(move || {
            [
                git(&work, &knot_env, &["push", "-q", &knot_push_url, "main"]),
                git(
                    &work,
                    &reference_env,
                    &["push", "-q", &reference_push_url, "main"],
                ),
            ]
        })
        .await
    };
    pushes.iter().for_each(|(ok, out)| {
        assert!(ok, "push failed:\n{out}");
    });

    let source_repo = RepoDid::new(REPO_DID).unwrap();
    let knot_objects: std::collections::BTreeSet<(String, u64)> = world
        .lfs
        .store
        .enumerate(&source_repo)
        .unwrap()
        .into_iter()
        .map(|object| (object.oid.as_str().to_string(), object.size.get()))
        .collect();
    let reference_objects: std::collections::BTreeSet<(String, u64)> = hex_object_files(&upstream)
        .into_iter()
        .map(|(name, size, _)| (name, size))
        .collect();
    assert_eq!(
        knot_objects, expected,
        "the knot store holds exactly the pushed object set"
    );
    assert_eq!(
        knot_objects, reference_objects,
        "both servers hold identical object sets after the same push"
    );

    let knot_clone_url = format!("{}/{OWNER_DID}/{REPO_NAME}", world.http_base);
    let (knot_dst, reference_dst) = {
        let base = scratch.path().to_path_buf();
        let knot_env = knot_env.clone();
        let reference_env = reference_env.clone();
        let knot_clone_url = knot_clone_url.clone();
        let reference_push_url = reference_push_url.clone();
        in_git_blocking(move || {
            (
                clone_and_pull(&base, &knot_clone_url, "knot-reader", &knot_env),
                clone_and_pull(
                    &base,
                    &reference_push_url,
                    "reference-reader",
                    &reference_env,
                ),
            )
        })
        .await
    };
    ["media.bin", "extra.bin"].iter().for_each(|file| {
        assert_eq!(
            std::fs::read(knot_dst.join(file)).unwrap(),
            std::fs::read(reference_dst.join(file)).unwrap(),
            "{file}: both servers must check out identical media"
        );
    });
    assert_eq!(std::fs::read(knot_dst.join("media.bin")).unwrap(), media);
    assert_eq!(std::fs::read(knot_dst.join("extra.bin")).unwrap(), second);

    let knot_removed = world
        .lfs
        .store
        .object_file(&source_repo, &second_oid)
        .unwrap()
        .unwrap()
        .1;
    std::fs::remove_file(knot_removed).unwrap();
    let removed = hex_object_files(&upstream)
        .into_iter()
        .filter(|(name, _, _)| name == second_oid.as_str())
        .map(|(_, _, path)| std::fs::remove_file(path).unwrap())
        .count();
    assert!(
        removed > 0,
        "the reference server holds the object to remove"
    );

    let verdicts = {
        let base = scratch.path().to_path_buf();
        let knot_env = knot_env.clone();
        let reference_env = reference_env.clone();
        let knot_push_url = knot_push_url.clone();
        in_git_blocking(move || {
            [
                pull_verdict(&base, &knot_clone_url, "knot-missing-http", &knot_env),
                pull_verdict(&base, &knot_push_url, "knot-missing-ssh", &knot_env),
                pull_verdict(
                    &base,
                    &reference_push_url,
                    "reference-missing",
                    &reference_env,
                ),
            ]
        })
        .await
    };
    let [(http_ok, http_out), (ssh_ok, ssh_out), (reference_ok, _)] = verdicts;
    assert!(
        !http_ok,
        "an http pull of a missing object must fail loudly, never succeed silently:\n{http_out}"
    );
    assert!(
        !ssh_ok,
        "an ssh pull of a missing object must fail loudly, never succeed silently:\n{ssh_out}"
    );
    assert!(
        reference_ok,
        "scutiger 0.3.0 answers noop for a missing download and the client silently \
         succeeds. This pin is the recorded reason knot answers download instead, so its \
         get-object 404 turns the pull into a loud failure. If the reference starts failing \
         loudly too, the divergence note can be retired."
    );
}

async fn lfs_batch(
    world: &World,
    auth: Option<&str>,
    op: &str,
    oid: &LfsOid,
    size: u64,
) -> (http::StatusCode, serde_json::Value) {
    let body = serde_json::json!({
        "operation": op,
        "transfers": ["basic"],
        "objects": [{ "oid": oid.as_str(), "size": size }],
        "hash_algo": "sha256",
    });
    let mut builder = http::Request::builder()
        .method("POST")
        .uri(format!("/{OWNER_DID}/{REPO_NAME}/info/lfs/objects/batch"))
        .header(http::header::CONTENT_TYPE, "application/vnd.git-lfs+json");
    if let Some(auth) = auth {
        builder = builder.header(http::header::AUTHORIZATION, auth);
    }
    let request = builder
        .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = world.router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn lfs_put(
    world: &World,
    auth: Option<&str>,
    oid: &LfsOid,
    bytes: Vec<u8>,
) -> http::StatusCode {
    let mut builder = http::Request::builder()
        .method("PUT")
        .uri(format!(
            "/{OWNER_DID}/{REPO_NAME}/info/lfs/objects/{}",
            oid.as_str()
        ))
        .header(http::header::CONTENT_LENGTH, bytes.len());
    if let Some(auth) = auth {
        builder = builder.header(http::header::AUTHORIZATION, auth);
    }
    let request = builder.body(axum::body::Body::from(bytes)).unwrap();
    world
        .router
        .clone()
        .oneshot(request)
        .await
        .unwrap()
        .status()
}

fn basic_auth(token: &str) -> String {
    let raw = base64::engine::general_purpose::STANDARD.encode(format!("x-tangled-token:{token}"));
    format!("Basic {raw}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lfs_http_push_stores_an_object_with_a_push_token() {
    let scratch = tempfile::tempdir().unwrap();
    let (_key_path, public_line) = keygen(scratch.path());
    let world = spawn_world(public_line).await;
    let repo = RepoDid::new(REPO_DID).unwrap();

    let payload: Vec<u8> = (0..4096u32)
        .map(|n| (n.wrapping_mul(17) % 251) as u8)
        .collect();
    let oid = LfsOid::from_digest(Sha256::digest(&payload).into());
    let size = payload.len() as u64;

    let bearer = format!(
        "Bearer {}",
        service_jwt("sh.tangled.repo.push", "lfs-http-push-1")
    );
    let (status, body) = lfs_batch(&world, Some(&bearer), "upload", &oid, size).await;
    assert_eq!(
        status,
        http::StatusCode::OK,
        "authenticated upload batch: {body}"
    );
    let href = body["objects"][0]["actions"]["upload"]["href"]
        .as_str()
        .unwrap_or_else(|| panic!("expected an upload action, got {body}"));
    assert_eq!(
        href,
        format!(
            "{}/{OWNER_DID}/{REPO_NAME}/info/lfs/objects/{}",
            world.http_base,
            oid.as_str()
        ),
        "upload href points at the object route on this knot"
    );
    assert_ne!(
        body["objects"][0]["authenticated"],
        serde_json::Value::Bool(true),
        "upload objects mustn't claim authenticated=true, else git-lfs sends the object put with no auth and loops on 401: {body}"
    );
    assert!(
        world.lfs.store.probe(&repo, &oid).unwrap().is_none(),
        "object must be absent before the put"
    );

    let put = lfs_put(&world, Some(&bearer), &oid, payload.clone()).await;
    assert_eq!(
        put,
        http::StatusCode::OK,
        "the same push token must authorize both the batch and the object put"
    );
    assert_eq!(
        world.lfs.store.probe(&repo, &oid).unwrap(),
        Some(LfsSize::new(size)),
        "the put object must be durable in the store"
    );

    let get = http::Request::builder()
        .method("GET")
        .uri(format!(
            "/{OWNER_DID}/{REPO_NAME}/info/lfs/objects/{}",
            oid.as_str()
        ))
        .body(axum::body::Body::empty())
        .unwrap();
    let response = world.router.clone().oneshot(get).await.unwrap();
    assert_eq!(
        response.status(),
        http::StatusCode::OK,
        "anonymous download"
    );
    let served = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        served.as_ref(),
        payload.as_slice(),
        "an anonymous reader sees the byte-identical object a push stored"
    );

    let payload2: Vec<u8> = (0..2048u32)
        .map(|n| (n.wrapping_mul(29) % 251) as u8)
        .collect();
    let oid2 = LfsOid::from_digest(Sha256::digest(&payload2).into());
    let basic = basic_auth(&service_jwt("sh.tangled.repo.push", "lfs-http-push-2"));
    let (status, body) =
        lfs_batch(&world, Some(&basic), "upload", &oid2, payload2.len() as u64).await;
    assert_eq!(
        status,
        http::StatusCode::OK,
        "basic-auth upload batch: {body}"
    );
    let put = lfs_put(&world, Some(&basic), &oid2, payload2.clone()).await;
    assert_eq!(
        put,
        http::StatusCode::OK,
        "a push token presented as the http basic password must authenticate the put"
    );
    assert_eq!(
        world.lfs.store.probe(&repo, &oid2).unwrap(),
        Some(LfsSize::new(payload2.len() as u64)),
        "the basic-authenticated object must be durable too"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lfs_http_push_rejects_missing_and_mismatched_credentials() {
    let scratch = tempfile::tempdir().unwrap();
    let (_key_path, public_line) = keygen(scratch.path());
    let world = spawn_world(public_line).await;

    let payload: Vec<u8> = (0..1024u32)
        .map(|n| (n.wrapping_mul(13) % 251) as u8)
        .collect();
    let oid = LfsOid::from_digest(Sha256::digest(&payload).into());
    let size = payload.len() as u64;

    let (status, _) = lfs_batch(&world, None, "upload", &oid, size).await;
    assert_eq!(
        status,
        http::StatusCode::UNAUTHORIZED,
        "an unauthenticated upload batch is challenged"
    );

    let wrong_method = format!(
        "Bearer {}",
        service_jwt("sh.tangled.repo.create", "lfs-http-neg-method")
    );
    let (status, _) = lfs_batch(&world, Some(&wrong_method), "upload", &oid, size).await;
    assert_eq!(
        status,
        http::StatusCode::UNAUTHORIZED,
        "a token bound to another method cannot authorize a push"
    );

    let stranger = format!(
        "Bearer {}",
        service_jwt_as(REPO_DID, "sh.tangled.repo.push", "lfs-http-neg-acl")
    );
    let (status, _) = lfs_batch(&world, Some(&stranger), "upload", &oid, size).await;
    assert_eq!(
        status,
        http::StatusCode::FORBIDDEN,
        "a valid push token from a did that cannot push is refused by the acl"
    );

    let put = lfs_put(&world, None, &oid, payload).await;
    assert_eq!(
        put,
        http::StatusCode::UNAUTHORIZED,
        "an unauthenticated object put is challenged"
    );
    assert!(
        world
            .lfs
            .store
            .probe(&RepoDid::new(REPO_DID).unwrap(), &oid)
            .unwrap()
            .is_none(),
        "no rejected request may leave an object behind"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_push_over_http_authenticates_and_lands_the_ref() {
    let scratch = tempfile::tempdir().unwrap();
    let (_key_path, public_line) = keygen(scratch.path());
    let world = spawn_world(public_line).await;

    let path_env = std::env::var("PATH").unwrap_or_default();
    let home = scratch.path().to_str().unwrap().to_string();
    let env: Vec<(String, String)> = [("PATH", &path_env), ("HOME", &home)]
        .map(|(key, value)| (key.to_string(), value.clone()))
        .to_vec();

    let work = scratch.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let (ok, out) = git(&work, &env, &["init", "-q", "-b", "main"]);
    assert!(ok, "init failed:\n{out}");
    std::fs::write(work.join("README.md"), "hello over http\n").unwrap();
    let (ok, out) = git(&work, &env, &["add", "-A"]);
    assert!(ok, "add failed:\n{out}");
    let (ok, out) = git(&work, &env, &["commit", "-q", "-m", "init over http"]);
    assert!(ok, "commit failed:\n{out}");

    let url = format!("{}/{OWNER_DID}/{REPO_NAME}", world.http_base);

    let (ok, out) = {
        let work = work.clone();
        let env = env.clone();
        let url = url.clone();
        in_git_blocking(move || git(&work, &env, &["push", "-q", &url, "main"])).await
    };
    assert!(
        !ok,
        "an unauthenticated http push must be refused, git reported success:\n{out}"
    );

    let token = service_jwt("sh.tangled.repo.push", "git-http-push-1");
    let header = format!(
        "http.extraHeader=Authorization: Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("x-tangled-token:{token}"))
    );
    let (ok, out) = {
        let work = work.clone();
        let env = env.clone();
        let url = url.clone();
        let header = header.clone();
        in_git_blocking(move || git(&work, &env, &["-c", &header, "push", "-q", &url, "main"]))
            .await
    };
    assert!(ok, "authenticated http push failed:\n{out}");

    let (ok, refs) = {
        let scratch = scratch.path().to_path_buf();
        let env = env.clone();
        let url = url.clone();
        in_git_blocking(move || git(&scratch, &env, &["ls-remote", &url])).await
    };
    assert!(
        ok && refs.contains("refs/heads/main"),
        "the pushed ref must be advertised to an anonymous reader:\n{refs}"
    );

    let wrong = format!(
        "http.extraHeader=Authorization: Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!(
            "x-tangled-token:{}",
            service_jwt("sh.tangled.repo.create", "git-http-push-neg")
        ))
    );
    std::fs::write(work.join("README.md"), "second write\n").unwrap();
    let (ok, out) = git(&work, &env, &["commit", "-q", "-am", "second"]);
    assert!(ok, "second commit failed:\n{out}");
    let (ok, out) = {
        let work = work.clone();
        let env = env.clone();
        let url = url.clone();
        let wrong = wrong.clone();
        in_git_blocking(move || git(&work, &env, &["-c", &wrong, "push", "-q", &url, "main"])).await
    };
    assert!(
        !ok,
        "a token bound to another method mustn't authorize a push:\n{out}"
    );
}

fn build_pack(work: &Path, env: &[(String, String)], tip: &str) -> Vec<u8> {
    let mut command = knot_fixtures::command(work);
    command
        .args(["pack-objects", "--revs", "--stdout", "--delta-base-offset"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    env.iter().for_each(|(key, value)| {
        command.env(key, value);
    });
    let mut child = command.spawn().expect("git pack-objects spawns");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{tip}\n").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "git pack-objects failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn build_receive_body(tip: &str, pack: &[u8]) -> Bytes {
    let mut command = format!("{} {tip} refs/heads/main", "0".repeat(tip.len())).into_bytes();
    command.push(0);
    command.extend_from_slice(b"report-status side-band-64k agent=knot-h3-test/0");
    command.push(b'\n');
    let mut body = common::pkt(&command);
    body.extend_from_slice(b"0000");
    body.extend_from_slice(pack);
    Bytes::from(body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_push_over_h3_authenticates_and_lands_the_ref() {
    let scratch = tempfile::tempdir().unwrap();
    let (_key_path, public_line) = keygen(scratch.path());
    let world = spawn(public_line, true).await;
    let edge = world.h3.as_ref().expect("the h3 edge is stood up");

    let path_env = std::env::var("PATH").unwrap_or_default();
    let home = scratch.path().to_str().unwrap().to_string();
    let env: Vec<(String, String)> = [("PATH", &path_env), ("HOME", &home)]
        .map(|(key, value)| (key.to_string(), value.clone()))
        .to_vec();

    let work = scratch.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let (ok, out) = git(&work, &env, &["init", "-q", "-b", "main"]);
    assert!(ok, "init failed:\n{out}");
    std::fs::write(work.join("README.md"), "hello over http3\n").unwrap();
    let (ok, out) = git(&work, &env, &["add", "-A"]);
    assert!(ok, "add failed:\n{out}");
    let (ok, out) = git(&work, &env, &["commit", "-q", "-m", "init over http3"]);
    assert!(ok, "commit failed:\n{out}");
    let (ok, tip) = git(&work, &env, &["rev-parse", "HEAD"]);
    assert!(ok, "rev-parse failed:\n{tip}");
    let tip = tip.trim().to_string();
    let pack = build_pack(&work, &env, &tip);

    let advert_uri =
        format!("https://localhost/{OWNER_DID}/{REPO_NAME}/info/refs?service=git-receive-pack");
    let receive_uri = format!("https://localhost/{OWNER_DID}/{REPO_NAME}/git-receive-pack");
    let warmup =
        format!("https://localhost/{OWNER_DID}/{REPO_NAME}/info/refs?service=git-upload-pack");
    const RECEIVE_CT: &str = "application/x-git-receive-pack-request";

    let (status, _) =
        common::h3_request(edge, Method::GET, advert_uri.clone(), &[], None, None).await;
    assert_eq!(
        status,
        http::StatusCode::UNAUTHORIZED,
        "an unauthenticated receive advertisement must be challenged over h3"
    );

    let token = basic_auth(&service_jwt("sh.tangled.repo.push", "git-h3-adv-1"));
    let (status, advert) = common::h3_request(
        edge,
        Method::GET,
        advert_uri,
        &[("authorization", token.as_str())],
        None,
        None,
    )
    .await;
    assert_eq!(
        status,
        http::StatusCode::OK,
        "an authenticated receive advertisement is served over h3"
    );
    assert!(
        String::from_utf8_lossy(&advert).contains("# service=git-receive-pack"),
        "the h3 receive advertisement includes the service banner"
    );

    let body = build_receive_body(&tip, &pack);
    let (status, _) = common::h3_request(
        edge,
        Method::POST,
        receive_uri.clone(),
        &[("content-type", RECEIVE_CT)],
        Some(body.clone()),
        Some(warmup.as_str()),
    )
    .await;
    assert_eq!(
        status,
        http::StatusCode::UNAUTHORIZED,
        "an unauthenticated receive-pack post must be challenged over h3"
    );

    let wrong = basic_auth(&service_jwt("sh.tangled.repo.create", "git-h3-neg"));
    let (status, _) = common::h3_request(
        edge,
        Method::POST,
        receive_uri.clone(),
        &[
            ("content-type", RECEIVE_CT),
            ("authorization", wrong.as_str()),
        ],
        Some(body.clone()),
        Some(warmup.as_str()),
    )
    .await;
    assert_eq!(
        status,
        http::StatusCode::UNAUTHORIZED,
        "a token bound to another method cannot authorize a receive-pack over h3"
    );

    let good = basic_auth(&service_jwt("sh.tangled.repo.push", "git-h3-push-1"));
    let (status, report) = common::h3_request(
        edge,
        Method::POST,
        receive_uri,
        &[
            ("content-type", RECEIVE_CT),
            ("authorization", good.as_str()),
        ],
        Some(body),
        Some(warmup.as_str()),
    )
    .await;
    assert_eq!(status, http::StatusCode::OK, "the authenticated h3 push");
    let report = String::from_utf8_lossy(&report);
    assert!(
        report.contains("unpack ok"),
        "the pack must unpack cleanly over h3:\n{report}"
    );
    assert!(
        report.contains("ok refs/heads/main"),
        "the ref update must be accepted over h3:\n{report}"
    );

    let repo = world.layout.open(&RepoDid::new(REPO_DID).unwrap()).unwrap();
    let landed = repo.references().unwrap().into_iter().any(|record| {
        record.name.as_str() == "refs/heads/main" && record.target.to_string() == tip
    });
    assert!(
        landed,
        "the ref pushed over h3 must be durable in the bare repo"
    );
}
