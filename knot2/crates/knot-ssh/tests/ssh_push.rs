use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use futures::stream::StreamExt;
use knot_atproto::Atproto;
use knot_cob::{CobHome, CobStore};
use knot_cobs::{CollaboratorsChange, Grant, MembersChange, Registration, RegistryChange};
use knot_git::{Layout, Repo};
use knot_index::Index;
use knot_pack::MaxWireBytes;
use knot_postreceive::LanguagesPushBudget;
use knot_runtime::{
    FakeDns, FakeHttp, HttpResponse, K256Signer, ManualClock, SeededEntropy, Signer, UnixMicros,
};
use knot_types::{
    AccountDid, KnotId, Oid, OwnerDid, RefName, RepoDid, RepoName, RepoRkey, UnixSeconds,
};
use tempfile::TempDir;
use tokio::net::TcpListener;
use url::Url;

const REPO_DID: &str = "did:plc:squid";
const REPO_NAME: &str = "anemone";
const OWNER_DID: &str = "did:plc:nel";
const PDS_HOST: &str = "pds.oyster.cafe";

fn git(cwd: &Path, env: &[(&str, &str)], args: &[&str]) -> (bool, String) {
    let mut command = knot_fixtures::command(cwd);
    command.args(args);
    env.iter().for_each(|(key, value)| {
        command.env(key, value);
    });
    let out = command.output().expect("git runs");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), combined)
}

fn keygen(dir: &Path, name: &str) -> (String, String) {
    let path = dir.join(name);
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
    assert!(
        out.status.success(),
        "ssh-keygen failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let public_line = std::fs::read_to_string(dir.join(format!("{name}.pub")))
        .unwrap()
        .trim()
        .to_string();
    (path.to_str().unwrap().to_string(), public_line)
}

fn did_document(signer: &K256Signer, did: &str, pds: &str) -> Vec<u8> {
    let multikey = knot_types::crypto::multikey(0xe7, signer.public_key().as_bytes());
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
            "serviceEndpoint": pds
        }]
    }))
    .unwrap()
}

fn list_records_body(lines: &[&str]) -> Vec<u8> {
    let records: Vec<_> = lines
        .iter()
        .map(|line| {
            serde_json::json!({
                "value": {
                    "$type": "sh.tangled.publicKey",
                    "key": line,
                    "name": "laptop",
                    "createdAt": "2026-06-08T00:00:00Z"
                }
            })
        })
        .collect();
    serde_json::to_vec(&serde_json::json!({ "records": records })).unwrap()
}

fn ok_body(body: Vec<u8>) -> HttpResponse {
    HttpResponse {
        status: http::StatusCode::OK,
        headers: http::HeaderMap::new(),
        body: bytes::Bytes::from(body),
    }
}

fn fake_dns() -> impl knot_runtime::DnsTxtResolver {
    FakeDns::new(|name: &str| {
        Ok(match name {
            "_atproto.nel.pet" => vec![format!("did={OWNER_DID}")],
            _ => Vec::new(),
        })
    })
}

fn not_found() -> HttpResponse {
    HttpResponse {
        status: http::StatusCode::NOT_FOUND,
        headers: http::HeaderMap::new(),
        body: bytes::Bytes::new(),
    }
}

fn fake_http(published_line: String) -> impl knot_runtime::HttpTransport {
    let signer = K256Signer::generate(&SeededEntropy::new(1));
    let pds = format!("https://{PDS_HOST}");
    FakeHttp::new(move |request| {
        let host = request.url.host_str().unwrap_or_default().to_string();
        let path = request.url.path().to_string();
        let body = if host == PDS_HOST {
            list_records_body(&[&published_line])
        } else if path.ends_with(REPO_DID) {
            did_document(&signer, REPO_DID, &pds)
        } else if path.ends_with(OWNER_DID) {
            did_document(&signer, OWNER_DID, &pds)
        } else {
            return Ok(not_found());
        };
        Ok(ok_body(body))
    })
}

fn multi_http(identities: HashMap<String, Vec<String>>) -> impl knot_runtime::HttpTransport {
    let signer = K256Signer::generate(&SeededEntropy::new(77));
    FakeHttp::new(move |request| {
        let host = request.url.host_str().unwrap_or_default().to_string();
        if host == "plc.directory" {
            let did = request.url.path().trim_start_matches('/').to_string();
            return Ok(ok_body(did_document(&signer, &did, "https://pds.test")));
        }
        if host == "pds.test" {
            let repo = request
                .url
                .query_pairs()
                .find(|(key, _)| key == "repo")
                .map(|(_, value)| value.into_owned())
                .unwrap_or_default();
            let lines = identities.get(&repo).cloned().unwrap_or_default();
            let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
            return Ok(ok_body(list_records_body(&refs)));
        }
        Ok(not_found())
    })
}

fn actor_for_seed(seed: u64) -> knot_types::ActorId {
    knot_types::ActorId::from_secp256k1(
        K256Signer::generate(&SeededEntropy::new(seed))
            .public_key()
            .as_bytes(),
    )
}

struct Server {
    _scan: TempDir,
    layout: Layout,
    repo_did: RepoDid,
    port: u16,
    events: Arc<knot_events::EventLog<ManualClock>>,
}

async fn spawn_server(
    published_line: String,
    max_pack_bytes: MaxWireBytes,
) -> (Server, Arc<Index>) {
    spawn_server_with(published_line, max_pack_bytes, true).await
}

async fn spawn_server_with(
    published_line: String,
    max_pack_bytes: MaxWireBytes,
    warm: bool,
) -> (Server, Arc<Index>) {
    let (server, index, _, _) = spawn_server_core(published_line, max_pack_bytes, warm, None).await;
    (server, index)
}

async fn spawn_server_core(
    published_line: String,
    max_pack_bytes: MaxWireBytes,
    warm: bool,
    lfs: Option<knot_lfs::LfsHandle>,
) -> (
    Server,
    Arc<Index>,
    tokio_util::sync::CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    let scan = tempfile::tempdir().unwrap();
    let meta_path = scan.path().join("meta");
    Repo::create(&meta_path).unwrap();
    let layout = Layout::new(scan.path().join("repos"));
    let repo_did = RepoDid::new(REPO_DID).unwrap();
    layout.create(&repo_did).unwrap();

    let signer = K256Signer::generate(&SeededEntropy::new(2));
    let meta = Repo::open(&meta_path).unwrap();
    let store = CobStore::new(&meta);
    store
        .create(
            &CobHome::from(&KnotId::new("did:web:nel.pet").unwrap()),
            &RegistryChange::Register(Registration {
                owner: OwnerDid::new(OWNER_DID).unwrap(),
                rkey: RepoRkey::new(REPO_NAME).unwrap(),
                name: RepoName::new(REPO_NAME).unwrap(),
                repo: repo_did.clone(),
                created_at: UnixSeconds::new(1),
            }),
            &signer,
            UnixSeconds::new(1),
        )
        .unwrap();

    let index = Arc::new(Index::new(meta_path, layout.clone()));
    if warm {
        index.rebuild().unwrap();
    }

    let atproto = Arc::new(
        Atproto::new(
            fake_http(published_line),
            ManualClock::new(UnixMicros::new(1_000_000_000)),
            KnotId::new("did:web:nel.pet").unwrap(),
            knot_atproto::PlcDirectory::new(Url::parse("https://plc.directory/").unwrap()).unwrap(),
        )
        .with_dns(Arc::new(fake_dns())),
    );

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
    let base = knot_ssh::SshState::new(
        layout.clone(),
        Arc::clone(&index),
        atproto,
        actor_for_seed(1),
        Arc::clone(&events),
        knot_types::KnotHostname::new("knot.test").unwrap(),
        knot_types::AppviewEndpoint::new("https://tangled.test").unwrap(),
        std::collections::BTreeSet::new(),
        knot_types::AdmissionPolicy::Closed,
        max_pack_bytes,
        LanguagesPushBudget::new(std::time::Duration::from_secs(2)),
        None,
    );
    let state = Arc::new(match lfs {
        Some(handle) => base.with_lfs(handle, 2),
        None => base,
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let serve_task = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            let _ = knot_ssh::serve_drained(listener, host_key, state, shutdown).await;
        }
    });

    (
        Server {
            _scan: scan,
            layout,
            repo_did,
            port,
            events,
        },
        index,
        shutdown,
        serve_task,
    )
}

fn ssh_command(key_path: &str) -> String {
    format!(
        "ssh -i {key_path} -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
         -o UserKnownHostsFile=/dev/null -o PreferredAuthentications=publickey -o BatchMode=yes"
    )
}

async fn git_ssh(cwd: &Path, key: &str, args: &[&str]) -> (bool, String) {
    let ssh = ssh_command(key);
    let cwd = cwd.to_path_buf();
    let owned: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let argv: Vec<&str> = owned.iter().map(String::as_str).collect();
        git(&cwd, &[("GIT_SSH_COMMAND", &ssh)], &argv)
    })
    .await
    .unwrap()
}

async fn push(work: &Path, url: &str, key: &str, refspecs: &[&str]) -> (bool, String) {
    let args: Vec<&str> = std::iter::once("push")
        .chain(std::iter::once(url))
        .chain(refspecs.iter().copied())
        .collect();
    git_ssh(work, key, &args).await
}

async fn clone(url: &str, key: &str, dest: &Path) -> (bool, String) {
    git_ssh(
        Path::new("/tmp"),
        key,
        &["clone", "-q", url, dest.to_str().unwrap()],
    )
    .await
}

fn seed_work(work: &Path) -> String {
    std::fs::create_dir_all(work).unwrap();
    git(work, &[], &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README.md"), "hello over ssh\n").unwrap();
    git(work, &[], &["add", "-A"]);
    git(work, &[], &["commit", "-q", "-m", "initial"]);
    let (ok, head) = git(work, &[], &["rev-parse", "HEAD"]);
    assert!(ok);
    head.trim().to_string()
}

fn seed_commits(work: &Path, count: usize) {
    std::fs::create_dir_all(work).unwrap();
    git(work, &[], &["init", "-q", "-b", "main"]);
    (0..count).for_each(|i| {
        std::fs::write(work.join("log.txt"), format!("line {i}\n")).unwrap();
        git(work, &[], &["add", "-A"]);
        git(work, &[], &["commit", "-q", "-m", &format!("c{i}")]);
    });
}

fn seed_cob(work: &Path, signer_seed: u64, subject: &str, home: &CobHome) -> (Oid, String, String) {
    let repo = Repo::open(work).unwrap();
    let signer = K256Signer::generate(&SeededEntropy::new(signer_seed));
    let created = CobStore::new(&repo)
        .create(
            home,
            &MembersChange::Add(Grant {
                subject: AccountDid::new(subject).unwrap(),
                added_by: AccountDid::new(OWNER_DID).unwrap(),
                created_at: UnixSeconds::new(1),
            }),
            &signer,
            UnixSeconds::new(1),
        )
        .unwrap();
    let cob_ref = format!(
        "refs/cobs/sh.tangled.knot.member/{}",
        created.object.oid().to_hex()
    );
    let spec = format!("{cob_ref}:{cob_ref}");
    (created.tip.oid(), cob_ref, spec)
}

fn main_tip(layout: &Layout, repo: &RepoDid) -> Option<Oid> {
    layout
        .open(repo)
        .unwrap()
        .find_ref(&RefName::new("refs/heads/main").unwrap())
        .unwrap()
}

fn ref_names(server: &Server) -> Vec<String> {
    server
        .layout
        .open(&server.repo_did)
        .unwrap()
        .references()
        .unwrap()
        .iter()
        .map(|record| record.name.as_str().to_string())
        .collect()
}

fn replay_bounds() -> knot_events::ReplayBounds {
    knot_events::ReplayBounds::new(
        knot_events::ReplayEvents::new(32).unwrap(),
        knot_events::ReplayBytes::new(16 << 20).unwrap(),
    )
}

async fn poll_for_event(
    events: &knot_events::EventLog<ManualClock>,
    nsid: &str,
) -> serde_json::Value {
    for _ in 0..50 {
        if let Some(payload) = events
            .replay(knot_events::EventCursor::START, replay_bounds())
            .events
            .into_iter()
            .find(|event| event.nsid == nsid)
            .map(|event| serde_json::to_value(&*event).unwrap()["event"].clone())
        {
            return payload;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("no {nsid} event was published within the polling window");
}

struct Fixture {
    scratch: TempDir,
    server: Server,
    index: Arc<Index>,
    key_path: String,
    url: String,
    work: PathBuf,
}

async fn fixture() -> Fixture {
    let scratch = tempfile::tempdir().unwrap();
    let (key_path, public_line) = keygen(scratch.path(), "client");
    let (server, index) = spawn_server(public_line, MaxWireBytes::new(1 << 30)).await;
    let url = format!(
        "ssh://git@127.0.0.1:{}/{OWNER_DID}/{REPO_NAME}",
        server.port
    );
    let work = scratch.path().join("work");
    Fixture {
        scratch,
        server,
        index,
        key_path,
        url,
        work,
    }
}

fn fetch_main_exit(clone_dir: &Path, ssh: &str, extra_git: &[&str]) -> Option<i32> {
    let mut args = vec!["-k", "3", "20", "git"];
    args.extend_from_slice(extra_git);
    args.extend_from_slice(&["fetch", "origin", "main"]);
    Command::new("timeout")
        .args(&args)
        .current_dir(clone_dir)
        .env("GIT_SSH_COMMAND", ssh)
        .status()
        .expect("timeout/git runs")
        .code()
}

async fn incremental_fetch_exit(
    seed_count: usize,
    extra_git: &'static [&'static str],
) -> Option<i32> {
    let scratch = tempfile::tempdir().unwrap();
    let (key_path, public_line) = keygen(scratch.path(), "client");
    let (server, _index) = spawn_server(public_line, MaxWireBytes::new(1 << 30)).await;
    let url = format!(
        "ssh://git@127.0.0.1:{}/{OWNER_DID}/{REPO_NAME}",
        server.port
    );

    let work = scratch.path().join("work");
    seed_commits(&work, seed_count);
    let (ok, out) = push(&work, &url, &key_path, &["main"]).await;
    assert!(ok, "seeding push must land:\n{out}");

    let clone_dir = scratch.path().join("clone");
    let (ok, out) = clone(&url, &key_path, &clone_dir).await;
    assert!(ok, "clone over ssh must succeed:\n{out}");

    git(
        &work,
        &[],
        &["commit", "-q", "--allow-empty", "-m", "advance"],
    );
    let (ok, out) = push(&work, &url, &key_path, &["main"]).await;
    assert!(ok, "advancing server tip must succeed:\n{out}");

    let ssh = ssh_command(&key_path);
    let exit = tokio::task::spawn_blocking(move || fetch_main_exit(&clone_dir, &ssh, extra_git))
        .await
        .unwrap();
    drop(server);
    exit
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incremental_fetch_over_ssh_completes() {
    let cases: [(&'static [&'static str], &str); 2] = [
        (
            &["-c", "protocol.version=0"],
            "diverged v0 fetch sends more than 32 haves and blocks on an ACK/NAK. Upload loop \
             answers each have-batch flush with a NAK instead of waiting for done, so it never \
             hangs",
        ),
        (
            &[],
            "git forwards GIT_PROTOCOL over ssh, so default fetch path negotiates with the v2 loop",
        ),
    ];
    futures::stream::iter(cases)
        .for_each(|(extra, rationale)| async move {
            assert_eq!(
                incremental_fetch_exit(50, extra).await,
                Some(0),
                "{rationale}"
            );
        })
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn addressing_variants_land() {
    let fx = fixture().await;
    let head = seed_work(&fx.work);
    let head_oid = Oid::from_hex(&head).unwrap();
    let port = fx.server.port;
    let variants = [
        format!("ssh://git@127.0.0.1:{port}/{OWNER_DID}/{REPO_NAME}"),
        format!("ssh://git@127.0.0.1:{port}/{OWNER_DID}/{REPO_NAME}.git"),
        format!("ssh://git@127.0.0.1:{port}/nel.pet/{REPO_NAME}"),
        format!("ssh://git@127.0.0.1:{port}/{REPO_DID}"),
    ];
    let fx = &fx;
    futures::stream::iter(variants)
        .for_each(|url| async move {
            let (ok, out) = push(&fx.work, &url, &fx.key_path, &["main"]).await;
            assert!(ok, "addressing {url} must resolve and push:\n{out}");
            assert_eq!(
                main_tip(&fx.server.layout, &fx.server.repo_did),
                Some(head_oid),
                "{url}: pushed commit must be the repository's main tip"
            );
        })
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_push_while_the_index_is_warming_is_refused() {
    let scratch = tempfile::tempdir().unwrap();
    let (key_path, public_line) = keygen(scratch.path(), "client");
    let (server, _index) = spawn_server_with(public_line, MaxWireBytes::new(1 << 30), false).await;
    let url = format!(
        "ssh://git@127.0.0.1:{}/{OWNER_DID}/{REPO_NAME}",
        server.port
    );

    let work = scratch.path().join("work");
    seed_work(&work);
    let (ok, out) = push(&work, &url, &key_path, &["main"]).await;
    assert!(
        !ok,
        "warming index must fail closed at the SSH boundary:\n{out}"
    );
    assert_eq!(
        main_tip(&server.layout, &server.repo_did),
        None,
        "no ref lands while index is warming"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unresolvable_targets_refused() {
    let fx = fixture().await;
    seed_work(&fx.work);
    let port = fx.server.port;

    let bad_name = format!("ssh://git@127.0.0.1:{port}/{OWNER_DID}/conch");
    let (ok, out) = push(&fx.work, &bad_name, &fx.key_path, &["main"]).await;
    assert!(
        !ok,
        "owner/name with no registry entry must be rejected, not silently routed:\n{out}"
    );

    fx.server
        .layout
        .create(&RepoDid::new("did:plc:clam").unwrap())
        .unwrap();
    let ghost_url = format!("ssh://git@127.0.0.1:{port}/did:plc:clam");
    let dest = fx.scratch.path().join("ghost");
    let (ok, out) = clone(&ghost_url, &fx.key_path, &dest).await;
    assert!(
        !ok,
        "repo present on disk but absent from registry mustn't be served by bare DID:\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_authorized_push_over_ssh_succeeds_and_a_clone_reads_it_back() {
    let fx = fixture().await;
    let head = seed_work(&fx.work);

    let (ok, out) = push(&fx.work, &fx.url, &fx.key_path, &["main"]).await;
    assert!(ok, "authorized push over ssh must succeed:\n{out}");
    assert_eq!(
        main_tip(&fx.server.layout, &fx.server.repo_did),
        Some(Oid::from_hex(&head).unwrap()),
        "pushed commit must be the repository's main tip"
    );

    let clone_dir = fx.scratch.path().join("clone");
    let (ok, out) = clone(&fx.url, &fx.key_path, &clone_dir).await;
    assert!(ok, "clone over ssh must succeed:\n{out}");
    assert!(
        clone_dir.join("README.md").exists(),
        "clone must check out the pushed file"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_authorized_push_emits_a_ref_update_event() {
    let fx = fixture().await;
    let head = seed_work(&fx.work);

    let (ok, out) = push(&fx.work, &fx.url, &fx.key_path, &["main"]).await;
    assert!(ok, "authorized push over ssh must succeed:\n{out}");

    let event = poll_for_event(&fx.server.events, "sh.tangled.git.refUpdate").await;
    assert_eq!(event["ref"], "refs/heads/main");
    assert_eq!(event["newSha"], head);
    assert_eq!(event["committerDid"], OWNER_DID);
    assert_eq!(event["ownerDid"], OWNER_DID);
    assert_eq!(event["meta"]["isDefaultRef"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_requesting_ssh_compression_clones_an_incompressible_pack() {
    let fx = fixture().await;
    std::fs::create_dir_all(&fx.work).unwrap();
    git(&fx.work, &[], &["init", "-q", "-b", "main"]);
    let mut state = 0x9e3779b97f4a7c15u64;
    let payload: Vec<u8> = std::iter::repeat_with(|| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state.to_le_bytes()
    })
    .take(32 * 1024)
    .flatten()
    .collect();
    std::fs::write(fx.work.join("noise.bin"), &payload).unwrap();
    git(&fx.work, &[], &["add", "-A"]);
    git(&fx.work, &[], &["commit", "-q", "-m", "noise"]);

    let (ok, out) = push(&fx.work, &fx.url, &fx.key_path, &["main"]).await;
    assert!(ok, "push must succeed:\n{out}");

    let dest = fx.scratch.path().join("compressed-clone");
    let ssh = format!("{} -o Compression=yes", ssh_command(&fx.key_path));
    let url = fx.url.clone();
    let dest_arg = dest.to_str().unwrap().to_string();
    let (ok, out) = tokio::task::spawn_blocking(move || {
        git(
            Path::new("/tmp"),
            &[("GIT_SSH_COMMAND", &ssh)],
            &["clone", "-q", &url, &dest_arg],
        )
    })
    .await
    .unwrap();
    assert!(
        ok,
        "clone with ssh compression requested must succeed:\n{out}"
    );
    assert_eq!(std::fs::read(dest.join("noise.bin")).unwrap(), payload);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_oversized_push_is_refused_at_the_ssh_boundary() {
    let scratch = tempfile::tempdir().unwrap();
    let (key_path, public_line) = keygen(scratch.path(), "client");
    let (server, _index) = spawn_server(public_line, MaxWireBytes::new(64)).await;
    let url = format!(
        "ssh://git@127.0.0.1:{}/{OWNER_DID}/{REPO_NAME}",
        server.port
    );

    let work = scratch.path().join("work");
    seed_work(&work);
    let (ok, out) = push(&work, &url, &key_path, &["main"]).await;
    assert!(
        !ok,
        "push larger than the configured limit must be refused:\n{out}"
    );
    assert!(
        ref_names(&server).is_empty(),
        "oversized push mustn't land any ref"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_up_to_date_push_over_ssh_is_accepted() {
    let fx = fixture().await;
    seed_work(&fx.work);

    let (ok, out) = push(&fx.work, &fx.url, &fx.key_path, &["main"]).await;
    assert!(ok, "first push must land:\n{out}");

    let (ok, out) = push(&fx.work, &fx.url, &fx.key_path, &["main"]).await;
    assert!(
        ok,
        "up-to-date no-op push must succeed instead of failing with a stream error:\n{out}"
    );
    assert!(
        out.contains("up-to-date") || out.contains("up to date"),
        "git must report branch is up to date:\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_denied_push_over_ssh_leaves_no_objects_in_the_live_odb() {
    let scratch = tempfile::tempdir().unwrap();
    let (_registered_path, registered_line) = keygen(scratch.path(), "registered");
    let (attacker_path, _attacker_line) = keygen(scratch.path(), "attacker");
    let (server, _index) = spawn_server(registered_line, MaxWireBytes::new(1 << 30)).await;
    let url = format!(
        "ssh://git@127.0.0.1:{}/{OWNER_DID}/{REPO_NAME}",
        server.port
    );

    let work = scratch.path().join("work");
    let head = seed_work(&work);
    let (ok, out) = push(&work, &url, &attacker_path, &["main"]).await;
    assert!(!ok, "unauthorized push must be rejected:\n{out}");

    let repo = server.layout.open(&server.repo_did).unwrap();
    assert!(
        repo.references().unwrap().is_empty(),
        "denied push must create no ref"
    );
    assert!(
        !repo.contains(Oid::from_hex(&head).unwrap()),
        "denied push must migrate no objects into the live odb"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ref_namespace_policy() {
    let fx = fixture().await;
    seed_work(&fx.work);

    let (ok, out) = push(
        &fx.work,
        &fx.url,
        &fx.key_path,
        &["main:refs/hidden/feature/main"],
    )
    .await;
    assert!(!ok, "push to refs/hidden/* must be rejected:\n{out}");
    assert!(
        ref_names(&fx.server).is_empty(),
        "forbidden-ref push must land nothing"
    );

    let (ok, out) = push(
        &fx.work,
        &fx.url,
        &fx.key_path,
        &["main:refs/notes/commits"],
    )
    .await;
    assert!(
        ok,
        "push to any non-reserved namespace must be accepted:\n{out}"
    );
    assert!(
        ref_names(&fx.server)
            .iter()
            .any(|name| name == "refs/notes/commits"),
        "pushed ref must land"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cob_ref_guard_lifecycle() {
    let fx = fixture().await;
    seed_work(&fx.work);
    let home = CobHome::from(&RepoDid::new(REPO_DID).unwrap());
    let foreign = CobHome::from(&RepoDid::new("did:plc:whelk").unwrap());

    let (ok, out) = push(&fx.work, &fx.url, &fx.key_path, &["main"]).await;
    assert!(ok, "head must land for the advertisement check:\n{out}");

    let (owned_tip, owned_ref, owned_spec) = seed_cob(&fx.work, 1, "did:plc:limpet", &home);
    let (ok, out) = push(&fx.work, &fx.url, &fx.key_path, &[owned_spec.as_str()]).await;
    assert!(
        ok,
        "COB ref signed by the repository key must verify and land over ssh:\n{out}"
    );

    let (_forged_tip, forged_ref, forged_spec) = seed_cob(&fx.work, 9, "did:plc:whelk", &home);
    let (ok, out) = push(&fx.work, &fx.url, &fx.key_path, &[forged_spec.as_str()]).await;
    assert!(
        !ok,
        "COB ref signed by a stranger must be refused at the receive boundary:\n{out}"
    );

    let (_transplant_tip, transplant_ref, transplant_spec) =
        seed_cob(&fx.work, 1, "did:plc:mussel", &foreign);
    let (ok, out) = push(&fx.work, &fx.url, &fx.key_path, &[transplant_spec.as_str()]).await;
    assert!(
        !ok,
        "same key signing for another repo's home must be refused on transplant:\n{out}"
    );

    let landed = ref_names(&fx.server);
    assert!(
        landed.contains(&owned_ref),
        "owner-signed COB ref must be stored: {landed:?}"
    );
    assert!(
        !landed.contains(&forged_ref),
        "stranger-signed COB ref must be absent: {landed:?}"
    );
    assert!(
        !landed.contains(&transplant_ref),
        "transplanted COB ref must be absent: {landed:?}"
    );

    let cob_name = RefName::new(&owned_ref).unwrap();
    let del = format!(":{owned_ref}");
    let (ok, out) = push(&fx.work, &fx.url, &fx.key_path, &[del.as_str()]).await;
    assert!(!ok, "deleting a COB ref must be refused:\n{out}");
    assert!(
        out.contains("append-only"),
        "rejection must name the append-only rule:\n{out}"
    );
    assert!(
        ref_names(&fx.server).contains(&owned_ref),
        "COB ref must survive the refused delete"
    );

    let repo = Repo::open(&fx.work).unwrap();
    CobStore::new(&repo)
        .update(
            &home,
            knot_types::CobId::new(owned_tip),
            &MembersChange::Add(Grant {
                subject: AccountDid::new("did:plc:bailey").unwrap(),
                added_by: AccountDid::new(OWNER_DID).unwrap(),
                created_at: UnixSeconds::new(2),
            }),
            &K256Signer::generate(&SeededEntropy::new(1)),
            UnixSeconds::new(2),
        )
        .unwrap();
    assert_ne!(
        repo.find_ref(&cob_name).unwrap(),
        Some(owned_tip),
        "local COB ref now points at a new, equally valid tip"
    );
    let (ok, out) = push(&fx.work, &fx.url, &fx.key_path, &[owned_spec.as_str()]).await;
    assert!(
        !ok,
        "re-pushing a moved COB ref must be refused instead of silently clobbered:\n{out}"
    );
    assert_eq!(
        fx.server
            .layout
            .open(&fx.server.repo_did)
            .unwrap()
            .find_ref(&cob_name)
            .unwrap(),
        Some(owned_tip),
        "live COB ref must still point at the original tip"
    );

    let (ok, advert) = git_ssh(Path::new("/tmp"), &fx.key_path, &["ls-remote", &fx.url]).await;
    assert!(ok, "ls-remote over ssh must succeed:\n{advert}");
    assert!(
        advert.contains("refs/heads/main"),
        "head must be advertised:\n{advert}"
    );
    assert!(
        !advert.contains("refs/cobs/"),
        "no refs/cobs/* may leak into the ssh advertisement:\n{advert}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn key_recognition_edge_cases() {
    let fx = fixture().await;
    let head = seed_work(&fx.work);
    let head_oid = Oid::from_hex(&head).unwrap();

    let (unregistered_path, _unregistered_line) = keygen(fx.scratch.path(), "unregistered");
    let two_ids = format!(
        "ssh -i {unregistered_path} -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
         -o UserKnownHostsFile=/dev/null -o PreferredAuthentications=publickey -o BatchMode=yes",
        fx.key_path
    );
    let (ok, out) = {
        let (work, url) = (fx.work.clone(), fx.url.clone());
        tokio::task::spawn_blocking(move || {
            git(
                &work,
                &[("GIT_SSH_COMMAND", &two_ids)],
                &["push", "-q", &url, "main"],
            )
        })
        .await
        .unwrap()
    };
    assert!(
        ok,
        "rejecting unregistered key must let client cycle to the registered one:\n{out}"
    );
    assert_eq!(
        main_tip(&fx.server.layout, &fx.server.repo_did),
        Some(head_oid)
    );

    let blob = russh::keys::ssh_key::PublicKey::from_openssh(
        &std::fs::read_to_string(fx.scratch.path().join("client.pub")).unwrap(),
    )
    .unwrap()
    .to_bytes()
    .unwrap();
    fx.index.cache_key(
        knot_types::OfferedKey::from_bytes(blob),
        &AccountDid::new("did:plc:whelk").unwrap(),
    );
    let (ok, out) = push(
        &fx.work,
        &fx.url,
        &fx.key_path,
        &["main:refs/heads/squat-check"],
    )
    .await;
    assert!(
        ok,
        "stranger who published the owner's key mustn't deny the owner's push:\n{out}"
    );
    assert_eq!(
        fx.server
            .layout
            .open(&fx.server.repo_did)
            .unwrap()
            .find_ref(&RefName::new("refs/heads/squat-check").unwrap())
            .unwrap(),
        Some(head_oid)
    );
}

#[test]
fn a_group_or_other_readable_host_key_is_refused_on_load() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("host");
    knot_ssh::load_or_create_host_key(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600,
        "freshly created host key is 0600"
    );

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let refused = knot_ssh::load_or_create_host_key(&path);
    assert!(
        matches!(refused, Err(knot_ssh::SshError::HostKey { .. })),
        "world-readable existing host key must be refused on load: {refused:?}"
    );
}

async fn launch(
    host_key_dir: &Path,
    layout: Layout,
    index: Arc<Index>,
    identities: HashMap<String, Vec<String>>,
) -> u16 {
    let atproto = Arc::new(Atproto::new(
        multi_http(identities),
        ManualClock::new(UnixMicros::new(1_000_000_000)),
        KnotId::new("did:web:nel.pet").unwrap(),
        knot_atproto::PlcDirectory::new(Url::parse("https://plc.directory/").unwrap()).unwrap(),
    ));
    std::fs::create_dir_all(host_key_dir).unwrap();
    let host_key = knot_ssh::load_or_create_host_key(&host_key_dir.join("host")).unwrap();
    let events = Arc::new(knot_events::EventLog::new(
        ManualClock::new(UnixMicros::new(1_000_000_000)),
        knot_events::ReplayBounds::new(
            knot_events::ReplayEvents::new(64).unwrap(),
            knot_events::ReplayBytes::new(16 << 20).unwrap(),
        ),
    ));
    let state = Arc::new(knot_ssh::SshState::new(
        layout,
        index,
        atproto,
        actor_for_seed(77),
        events,
        knot_types::KnotHostname::new("knot.test").unwrap(),
        knot_types::AppviewEndpoint::new("https://tangled.test").unwrap(),
        std::collections::BTreeSet::new(),
        knot_types::AdmissionPolicy::Closed,
        MaxWireBytes::new(1 << 30),
        LanguagesPushBudget::new(std::time::Duration::from_secs(2)),
        None,
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = knot_ssh::serve_on_socket(listener, host_key, state).await;
    });
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_collaborator_pushes_its_repo_but_a_recognized_key_is_denied_on_a_repo_it_has_no_grant_on()
 {
    const REPO_A: &str = "did:plc:squid";
    const REPO_B: &str = "did:plc:clam";
    const OWNER: &str = "did:plc:nel";
    const COLLAB: &str = "did:plc:olaren";

    let scratch = tempfile::tempdir().unwrap();
    let (owner_key, owner_line) = keygen(scratch.path(), "owner");
    let (collab_key, collab_line) = keygen(scratch.path(), "collab");

    let meta_path = scratch.path().join("meta");
    Repo::create(&meta_path).unwrap();
    let layout = Layout::new(scratch.path().join("repos"));
    let repo_a = RepoDid::new(REPO_A).unwrap();
    let repo_b = RepoDid::new(REPO_B).unwrap();
    let git_a = layout.create(&repo_a).unwrap();
    layout.create(&repo_b).unwrap();

    let signer = K256Signer::generate(&SeededEntropy::new(2));
    let meta = Repo::open(&meta_path).unwrap();
    let store = CobStore::new(&meta);
    let knot_home = CobHome::from(&KnotId::new("did:web:nel.pet").unwrap());
    let reg = store
        .create(
            &knot_home,
            &RegistryChange::Register(Registration {
                owner: OwnerDid::new(OWNER).unwrap(),
                rkey: RepoRkey::new("anemone").unwrap(),
                name: RepoName::new("anemone").unwrap(),
                repo: repo_a.clone(),
                created_at: UnixSeconds::new(1),
            }),
            &signer,
            UnixSeconds::new(1),
        )
        .unwrap();
    store
        .update(
            &knot_home,
            reg.object,
            &RegistryChange::Register(Registration {
                owner: OwnerDid::new(OWNER).unwrap(),
                rkey: RepoRkey::new("barnacle").unwrap(),
                name: RepoName::new("barnacle").unwrap(),
                repo: repo_b.clone(),
                created_at: UnixSeconds::new(2),
            }),
            &signer,
            UnixSeconds::new(2),
        )
        .unwrap();
    store
        .create(
            &knot_home,
            &MembersChange::Add(Grant {
                subject: AccountDid::new(COLLAB).unwrap(),
                added_by: AccountDid::new(OWNER).unwrap(),
                created_at: UnixSeconds::new(1),
            }),
            &signer,
            UnixSeconds::new(1),
        )
        .unwrap();
    CobStore::new(&git_a)
        .create(
            &CobHome::from(&repo_a),
            &CollaboratorsChange::Add(Grant {
                subject: AccountDid::new(COLLAB).unwrap(),
                added_by: AccountDid::new(OWNER).unwrap(),
                created_at: UnixSeconds::new(1),
            }),
            &signer,
            UnixSeconds::new(1),
        )
        .unwrap();

    let index = Arc::new(Index::new(meta_path, layout.clone()));
    index.rebuild().unwrap();
    index.warm_collaborators();

    let identities = HashMap::from([
        (OWNER.to_string(), vec![owner_line]),
        (COLLAB.to_string(), vec![collab_line]),
    ]);
    let port = launch(
        &scratch.path().join("hostkey"),
        layout.clone(),
        Arc::clone(&index),
        identities,
    )
    .await;

    let work_a = scratch.path().join("work_a");
    let head_a = seed_work(&work_a);
    let url_a = format!("ssh://git@127.0.0.1:{port}/{REPO_A}");
    let (ok, out) = push(&work_a, &url_a, &collab_key, &["main"]).await;
    assert!(
        ok,
        "collaborator must push the repo it collaborates on:\n{out}"
    );
    assert_eq!(
        main_tip(&layout, &repo_a),
        Some(Oid::from_hex(&head_a).unwrap()),
        "collaborator's commit must be repo A's main tip"
    );

    let work_b = scratch.path().join("work_b");
    seed_work(&work_b);
    let url_b = format!("ssh://git@127.0.0.1:{port}/{REPO_B}");
    let (denied, out) = push(&work_b, &url_b, &collab_key, &["main"]).await;
    assert!(
        !denied,
        "key recognized via repo A but with no grant on repo B must be denied, recognition is \
         not authorization:\n{out}"
    );
    assert!(
        main_tip(&layout, &repo_b).is_none(),
        "denied cross-repo push must land nothing on repo B"
    );

    let work_owner = scratch.path().join("work_owner_b");
    let head_owner = seed_work(&work_owner);
    let (ok, out) = push(&work_owner, &url_b, &owner_key, &["main"]).await;
    assert!(ok, "owner must push to repo B:\n{out}");
    assert_eq!(
        main_tip(&layout, &repo_b),
        Some(Oid::from_hex(&head_owner).unwrap()),
        "owner's push to repo B must land, isolating the collaborator's denial as authorization"
    );
}

fn ssh_bare(key_path: &str, port: u16) -> (bool, String) {
    let out = Command::new("ssh")
        .args([
            "-i",
            key_path,
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "PreferredAuthentications=publickey",
            "-o",
            "BatchMode=yes",
            "-p",
            &port.to_string(),
            "git@127.0.0.1",
        ])
        .output()
        .expect("ssh runs");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bare_ssh_session_greets_the_recognized_user() {
    let fx = fixture().await;
    let port = fx.server.port;
    let key_path = fx.key_path.clone();
    let (_ok, out) = tokio::task::spawn_blocking(move || ssh_bare(&key_path, port))
        .await
        .unwrap();
    assert!(
        out.contains("@nel.pet"),
        "greeting resolves and addresses the user by handle:\n{out}"
    );
    assert!(out.contains("knot.test"), "greeting names the knot:\n{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_push_to_a_new_branch_offers_a_pull_request_link() {
    let fx = fixture().await;
    seed_work(&fx.work);

    let (ok, out) = push(&fx.work, &fx.url, &fx.key_path, &["main"]).await;
    assert!(ok, "seeding main must land:\n{out}");

    git(&fx.work, &[], &["checkout", "-q", "-b", "feature"]);
    std::fs::write(fx.work.join("feature.txt"), "work\n").unwrap();
    git(&fx.work, &[], &["add", "-A"]);
    git(&fx.work, &[], &["commit", "-q", "-m", "feature work"]);

    let (ok, out) = push(&fx.work, &fx.url, &fx.key_path, &["feature"]).await;
    assert!(ok, "feature-branch push must land:\n{out}");
    assert!(
        out.contains("https://tangled.test/nel.pet/anemone/pulls/new"),
        "new non-default branch is answered with a pull-request link:\n{out}"
    );
    assert!(
        out.contains("sourceBranch=feature") && out.contains("targetBranch=main"),
        "link points the new branch at the default:\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_verbose_ci_push_option_reports_a_clean_pipeline() {
    let fx = fixture().await;
    std::fs::create_dir_all(fx.work.join(".tangled/workflows")).unwrap();
    git(&fx.work, &[], &["init", "-q", "-b", "main"]);
    std::fs::write(
        fx.work.join(".tangled/workflows/ci.yml"),
        "engine: nixery.dev/x\nwhen:\n  - event: push\n    branch: ['**']\n",
    )
    .unwrap();
    git(&fx.work, &[], &["add", "-A"]);
    git(&fx.work, &[], &["commit", "-q", "-m", "add ci"]);

    let (ok, out) = push(
        &fx.work,
        &fx.url,
        &fx.key_path,
        &["--push-option=verbose-ci", "main"],
    )
    .await;
    assert!(ok, "push with a push option must land:\n{out}");
    assert!(
        out.contains("no diagnostics"),
        "verbose-ci reports clean compile over the sideband:\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_archive_remote_over_ssh_streams_a_tar_of_the_tree() {
    let fx = fixture().await;
    seed_work(&fx.work);
    let (ok, out) = push(&fx.work, &fx.url, &fx.key_path, &["main"]).await;
    assert!(ok, "seeding push must land before archiving:\n{out}");

    let out_tar = fx.scratch.path().join("archive.tar");
    let (ok, out) = git_ssh(
        &fx.work,
        &fx.key_path,
        &[
            "archive",
            "--format=tar",
            "--remote",
            &fx.url,
            "-o",
            out_tar.to_str().unwrap(),
            "HEAD",
        ],
    )
    .await;
    assert!(ok, "git archive --remote over ssh must succeed:\n{out}");

    let tar = std::fs::read(&out_tar).unwrap();
    assert!(
        tar.windows(b"README.md".len()).any(|w| w == b"README.md"),
        "archived tar must contain the README.md entry"
    );
}

fn pkt(payload: &[u8]) -> Vec<u8> {
    let mut framed = format!("{:04x}", payload.len() + 4).into_bytes();
    framed.extend_from_slice(payload);
    framed
}

fn pkt_text(line: &str) -> Vec<u8> {
    pkt(format!("{line}\n").as_bytes())
}

fn read_until(reader: &mut impl std::io::Read, needle: &[u8], buffer: &mut Vec<u8>) {
    std::iter::from_fn(|| {
        let mut byte = [0u8; 1];
        match reader.read(&mut byte) {
            Ok(0) | Err(_) => None,
            Ok(_) => {
                buffer.push(byte[0]);
                Some(buffer.ends_with(needle))
            }
        }
    })
    .find(|done| *done)
    .expect("the session must answer before closing the stream");
}

fn trickled_lfs_upload(
    key_path: &str,
    port: u16,
    body: &[u8],
    oid: &str,
    midway: std::sync::mpsc::Sender<()>,
) -> (bool, String) {
    use std::io::Write;
    let mut child = Command::new("ssh")
        .args([
            "-i",
            key_path,
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "PreferredAuthentications=publickey",
            "-o",
            "BatchMode=yes",
            "-p",
            &port.to_string(),
            "git@127.0.0.1",
            &format!("git-lfs-transfer '{OWNER_DID}/{REPO_NAME}' upload"),
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("ssh runs");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut transcript = Vec::new();

    read_until(&mut stdout, b"version=1\n0000", &mut transcript);

    let (first, second) = body.split_at(body.len() / 2);
    stdin
        .write_all(&pkt_text(&format!("put-object {oid}")))
        .unwrap();
    stdin
        .write_all(&pkt_text(&format!("size={}", body.len())))
        .unwrap();
    stdin.write_all(b"0001").unwrap();
    first.chunks(32 * 1024).for_each(|chunk| {
        stdin.write_all(&pkt(chunk)).unwrap();
    });
    stdin.flush().unwrap();
    midway.send(()).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(900));

    second.chunks(32 * 1024).for_each(|chunk| {
        stdin.write_all(&pkt(chunk)).unwrap();
    });
    stdin.write_all(b"0000").unwrap();
    stdin.flush().unwrap();
    read_until(&mut stdout, b"status 200\n0000", &mut transcript);

    stdin.write_all(&pkt_text("quit")).unwrap();
    stdin.write_all(b"0000").unwrap();
    stdin.flush().unwrap();
    drop(stdin);
    use std::io::Read;
    let _ = stdout.read_to_end(&mut transcript);
    let status = child.wait().expect("ssh exits");
    (
        status.success(),
        String::from_utf8_lossy(&transcript).into_owned(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_drains_an_in_flight_lfs_transfer_before_exit() {
    use knot_lfs::LfsStore;
    use sha2::Digest;
    let scratch = tempfile::tempdir().unwrap();
    let (key_path, public_line) = keygen(scratch.path(), "drain");
    let lfs_dir = scratch.path().join("lfs");
    std::fs::create_dir_all(&lfs_dir).unwrap();
    let handle = knot_lfs::LfsHandle::open(
        knot_lfs::LfsStorePath::new(&lfs_dir),
        knot_lfs::LfsSize::new(1 << 30),
        knot_lfs::FreeSpaceFloor::new(0),
    )
    .unwrap();
    let (server, _index, shutdown, serve_task) = spawn_server_core(
        public_line,
        MaxWireBytes::new(1 << 20),
        true,
        Some(handle.clone()),
    )
    .await;

    let body: Vec<u8> = (0..1_048_576u32).map(|n| (n % 251) as u8).collect();
    let oid = knot_lfs::LfsOid::from_digest(sha2::Sha256::digest(&body).into());
    let (midway_tx, midway_rx) = std::sync::mpsc::channel();

    let client = {
        let key_path = key_path.clone();
        let oid = oid.clone();
        let port = server.port;
        tokio::task::spawn_blocking(move || {
            trickled_lfs_upload(&key_path, port, &body, oid.as_str(), midway_tx)
        })
    };

    tokio::task::spawn_blocking(move || {
        midway_rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("the upload must reach its midway point")
    })
    .await
    .unwrap();

    shutdown.cancel();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(
        !serve_task.is_finished(),
        "the listener must keep draining while a transfer is in flight"
    );

    let (ok, transcript) = client.await.unwrap();
    assert!(
        ok,
        "the in-flight upload must finish cleanly across the shutdown:\n{transcript}"
    );
    assert!(
        transcript.contains("status 200"),
        "the server must acknowledge the drained upload:\n{transcript}"
    );

    tokio::time::timeout(std::time::Duration::from_secs(10), serve_task)
        .await
        .expect("the drained listener must exit promptly once transfers finish")
        .unwrap();

    let repo_did = RepoDid::new(REPO_DID).unwrap();
    assert_eq!(
        handle
            .store
            .probe(&repo_did, &oid)
            .unwrap()
            .map(|size| size.get()),
        Some(1_048_576),
        "the drained upload must be durable"
    );

    let (connected, _) = {
        let key_path = key_path.clone();
        let port = server.port;
        tokio::task::spawn_blocking(move || ssh_bare(&key_path, port))
            .await
            .unwrap()
    };
    assert!(
        !connected,
        "a connection after shutdown must be refused, the drain only covers in-flight work"
    );
}
