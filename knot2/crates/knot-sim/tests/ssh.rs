use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use futures::stream::StreamExt;
use knot_atproto::Atproto;
use knot_cob::{CobHome, CobStore};
use knot_cobs::{Registration, RegistryChange};
use knot_git::{Layout, Repo};
use knot_runtime::{
    FakeHttp, HttpResponse, K256Signer, ManualClock, SeededEntropy, Signer, UnixMicros,
};
use knot_types::{KnotId, OwnerDid, RepoDid, RepoName, RepoRkey, UnixSeconds};
use tempfile::TempDir;
use tokio::net::TcpListener;
use url::Url;

const REPO_DID: &str = "did:plc:squid";
const REPO_NAME: &str = "anemone";
const OWNER_DID: &str = "did:plc:nel";
const PDS_HOST: &str = "pds.oyster.cafe";
const KNOT_DID: &str = "did:web:nel.pet";
const PINNED_DATE: &str = "2026-06-20T12:00:00+00:00";

fn git(cwd: &Path, env: &[(&str, &str)], args: &[&str]) -> (bool, String) {
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

fn did_document(signer: &K256Signer, did: &str) -> Vec<u8> {
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
                "createdAt": "2026-06-08T00:00:00Z"
            }
        }]
    }))
    .unwrap()
}

fn fake_http(published_line: String) -> impl knot_runtime::HttpTransport {
    let signer = K256Signer::generate(&SeededEntropy::new(1));
    FakeHttp::new(move |request| {
        let host = request.url.host_str().unwrap_or_default().to_string();
        let path = request.url.path().to_string();
        let body = if host == PDS_HOST {
            list_records_body(&published_line)
        } else if path.ends_with(OWNER_DID) {
            did_document(&signer, OWNER_DID)
        } else if path.ends_with(REPO_DID) {
            did_document(&signer, REPO_DID)
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

async fn spawn(published_line: String) -> Server {
    let scan = tempfile::tempdir().unwrap();
    let meta_path = scan.path().join("meta");
    Repo::create(&meta_path).unwrap();
    let layout = Layout::new(scan.path().join("repos"));
    let repo_did = RepoDid::new(REPO_DID).unwrap();
    layout.create(&repo_did).unwrap();

    let signer = K256Signer::generate(&SeededEntropy::new(2));
    let meta = Repo::open(&meta_path).unwrap();
    CobStore::new(&meta)
        .create(
            &CobHome::from(&KnotId::new(KNOT_DID).unwrap()),
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

    let index = Arc::new(knot_index::Index::new(meta_path, layout.clone()));
    index.rebuild().unwrap();

    let atproto = Arc::new(Atproto::new(
        fake_http(published_line),
        ManualClock::new(UnixMicros::new(1_000_000_000)),
        KnotId::new(KNOT_DID).unwrap(),
        knot_atproto::PlcDirectory::new(Url::parse("https://plc.directory/").unwrap()).unwrap(),
    ));
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
    let state = Arc::new(knot_ssh::SshState::new(
        layout.clone(),
        index,
        atproto,
        actor_for_seed(1),
        Arc::clone(&events),
        knot_types::KnotHostname::new("knot.test").unwrap(),
        knot_types::AppviewEndpoint::new("https://tangled.test").unwrap(),
        std::collections::BTreeSet::new(),
        knot_types::AdmissionPolicy::Closed,
        knot_xrpc::MaxWireBytes::new(1 << 30),
        knot_xrpc::LanguagesPushBudget::new(std::time::Duration::from_secs(2)),
        None,
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = knot_ssh::serve_on_socket(listener, host_key, state).await;
    });
    Server {
        _scan: scan,
        layout,
        repo_did,
        port,
        events,
    }
}

fn ssh_command(key_path: &str) -> String {
    format!(
        "ssh -i {key_path} -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
         -o UserKnownHostsFile=/dev/null -o PreferredAuthentications=publickey -o BatchMode=yes"
    )
}

fn seed_work(work: &Path) -> String {
    std::fs::create_dir_all(work).unwrap();
    git(work, &[], &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README.md"), "hello over the simulated ssh\n").unwrap();
    git(work, &[], &["add", "-A"]);
    git(work, &[], &["commit", "-q", "-m", "initial"]);
    let (ok, head) = git(work, &[], &["rev-parse", "HEAD"]);
    assert!(ok);
    head.trim().to_string()
}

async fn push_once(scratch: &Path) -> (String, Option<knot_types::Oid>, serde_json::Value) {
    let (key_path, public_line) = keygen(scratch);
    let server = spawn(public_line).await;
    let url = format!(
        "ssh://git@127.0.0.1:{}/{OWNER_DID}/{REPO_NAME}",
        server.port
    );
    let ssh = ssh_command(&key_path);
    let work = scratch.join("work");
    let head = seed_work(&work);

    let (ok, out) = tokio::task::spawn_blocking(move || {
        git(
            &work,
            &[("GIT_SSH_COMMAND", &ssh)],
            &["push", "-q", &url, "main"],
        )
    })
    .await
    .unwrap();
    assert!(ok, "simulated ssh server must accept push:\n{out}");

    let stored = server
        .layout
        .open(&server.repo_did)
        .unwrap()
        .find_ref(&knot_types::RefName::new("refs/heads/main").unwrap())
        .unwrap();
    let event = poll_for_event(&server.events).await;
    drop(server);
    (head, stored, event)
}

fn replay_bounds() -> knot_events::ReplayBounds {
    knot_events::ReplayBounds::new(
        knot_events::ReplayEvents::new(32).unwrap(),
        knot_events::ReplayBytes::new(16 << 20).unwrap(),
    )
}

async fn poll_for_event(events: &knot_events::EventLog<ManualClock>) -> serde_json::Value {
    futures::stream::iter(0..100)
        .then(|_| async {
            let hit = events
                .replay(knot_events::EventCursor::START, replay_bounds())
                .events
                .into_iter()
                .find(|event| event.nsid == "sh.tangled.git.refUpdate")
                .map(|event| serde_json::to_value(&*event).unwrap()["event"].clone());
            if hit.is_none() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            hit
        })
        .filter_map(|hit| async move { hit })
        .boxed()
        .next()
        .await
        .expect("refUpdate event must be published within polling window")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_simulated_ssh_write_path_lands_a_seed_deterministic_tip() {
    let first_dir = tempfile::tempdir().unwrap();
    let (first_head, first_stored, first_event) = push_once(first_dir.path()).await;

    let second_dir = tempfile::tempdir().unwrap();
    let (second_head, second_stored, second_event) = push_once(second_dir.path()).await;

    let tip = knot_types::Oid::from_hex(&first_head).unwrap();
    assert_eq!(
        first_stored,
        Some(tip),
        "pushed commit must be the repository's main tip"
    );
    assert_eq!(
        first_head, second_head,
        "two independent runs of the simulated ssh push must produce same commit oid"
    );
    assert_eq!(
        first_stored, second_stored,
        "assembled-against-doubles ssh write path is logically reproducible"
    );

    assert_eq!(first_event["ref"], "refs/heads/main");
    assert_eq!(first_event["newSha"], first_head);
    assert_eq!(
        first_event["newSha"], second_event["newSha"],
        "ref-update event the push emits is seed-stable too"
    );
}
