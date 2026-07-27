use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use knot_atproto::Atproto;
use knot_cob::{CobHome, CobStore};
use knot_cobs::{Registration, RegistryChange};
use knot_git::{Layout, Repo};
use knot_index::Index;
use knot_runtime::{
    FakeHttp, HttpResponse, K256Signer, ManualClock, SeededEntropy, Signer, UnixMicros,
};
use knot_types::{
    AdmissionPolicy, KnotHostname, KnotId, OwnerDid, RepoDid, RepoName, RepoRkey, UnixSeconds,
};
use tokio::net::TcpListener;
use url::Url;

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const REPO_DID: &str = "did:plc:squid";
const REPO_NAME: &str = "anemone";
const OWNER_DID: &str = "did:plc:nel";
const PDS_HOST: &str = "pds.oyster.cafe";

fn apply_decay(ms: isize) {
    unsafe {
        let _ = tikv_jemalloc_ctl::raw::write(b"arenas.dirty_decay_ms\0", ms);
        if let Ok(narenas) = tikv_jemalloc_ctl::raw::read::<u32>(b"arenas.narenas\0") {
            (0..narenas).for_each(|arena| {
                let name = format!("arena.{arena}.dirty_decay_ms\0");
                let _ = tikv_jemalloc_ctl::raw::write(name.as_bytes(), ms);
            });
        }
    }
}

async fn govern_decay() {
    unsafe {
        let _ = tikv_jemalloc_ctl::raw::write(b"background_thread\0", true);
    }
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    let mut applied = knot_resource::target_decay();
    apply_decay(applied.ms());
    loop {
        ticker.tick().await;
        let target = knot_resource::target_decay();
        if target != applied {
            apply_decay(target.ms());
            applied = target;
        }
    }
}

fn vm_hwm_mib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kb| kb.parse::<u64>().ok())
        .map(|kb| kb / 1024)
        .unwrap_or(0)
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

fn list_records_body(published_line: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "records": [{
            "value": {
                "$type": "sh.tangled.publicKey",
                "key": published_line,
                "name": "laptop",
                "createdAt": "2026-06-08T00:00:00Z"
            }
        }]
    }))
    .unwrap()
}

fn ok_body(body: Vec<u8>) -> HttpResponse {
    HttpResponse {
        status: http::StatusCode::OK,
        headers: http::HeaderMap::new(),
        body: bytes::Bytes::from(body),
    }
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
            list_records_body(&published_line)
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

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let port: u16 = args
        .next()
        .expect("usage: ephemeral_knot <port> <client-pubkey-file>")
        .parse()
        .expect("port");
    let pubkey_path = args.next().expect("client-pubkey-file");
    let published_line = std::fs::read_to_string(&pubkey_path)
        .expect("read client pubkey")
        .trim()
        .to_string();

    let max_threads = std::env::var("KNOT_MAX_THREADS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok());
    knot_resource::init(knot_resource::Ceilings {
        max_threads: max_threads.map(knot_resource::ThreadCount::new),
        max_memory: None,
    });

    let scan = tempfile::tempdir().expect("scan tempdir");
    let meta_path = scan.path().join("meta");
    Repo::create(&meta_path).unwrap();
    let layout = Layout::new(scan.path().join("repos"));
    let repo_did = RepoDid::new(REPO_DID).unwrap();
    layout.create(&repo_did).unwrap();

    let signer = K256Signer::generate(&SeededEntropy::new(2));
    let meta = Repo::open(&meta_path).unwrap();
    CobStore::new(&meta)
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
    index.rebuild().unwrap();

    let atproto = Arc::new(Atproto::new(
        fake_http(published_line),
        ManualClock::new(UnixMicros::new(1_000_000_000)),
        KnotId::new("did:web:nel.pet").unwrap(),
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
    let actor = knot_types::ActorId::from_secp256k1(
        K256Signer::generate(&SeededEntropy::new(1))
            .public_key()
            .as_bytes(),
    );
    let state = Arc::new(knot_ssh::SshState::new(
        layout,
        index,
        atproto,
        actor,
        events,
        KnotHostname::new("knot.test").unwrap(),
        knot_types::AppviewEndpoint::new("https://tangled.test").unwrap(),
        BTreeSet::new(),
        AdmissionPolicy::Closed,
        knot_pack::MaxWireBytes::new(1 << 34),
        knot_postreceive::LanguagesPushBudget::new(Duration::from_secs(2)),
        None,
    ));

    let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    let bound = listener.local_addr().unwrap().port();
    tokio::spawn(govern_decay());
    tokio::spawn(async move {
        let _ = knot_ssh::serve_on_socket(listener, host_key, state).await;
    });

    println!("READY pid={} port={}", std::process::id(), bound);
    let mut reporter = tokio::time::interval(Duration::from_secs(1));
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = async {
            loop {
                reporter.tick().await;
                println!("VmHWM {} MiB", vm_hwm_mib());
            }
        } => {}
    }
    drop(scan);
}
