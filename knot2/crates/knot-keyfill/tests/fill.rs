use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use knot_atproto::Atproto;
use knot_cob::{CobHome, CobStore};
use knot_cobs::{Grant, MembersChange, Registration, RegistryChange};
use knot_git::{Layout, Repo};
use knot_index::{Coverage, Index, KeyReprieve, KeyTtl, Resolved, SweepFloor};
use knot_keyfill::{
    AccountBudget, BusyRetry, Cursors, Pace, SettleFloor, SettledPause, StalledBackoff, fill_once,
};
use knot_resource::{Burst, HostKey, HostPacer, RateLimit, RefillMicros, Slots};
use knot_runtime::{
    FakeHttp, HttpRequest, HttpResponse, K256Signer, ManualClock, NetworkError, SeededEntropy,
    Signer, UnixMicros,
};
use knot_types::{
    AccountDid, KnotId, OfferedKey, OwnerDid, RepoDid, RepoName, RepoRkey, UnixSeconds,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use url::Url;

const KNOT_DID: &str = "did:web:nel.pet";
const NEL: &str = "did:plc:nel";
const OLAREN: &str = "did:plc:olaren";
const TEQ: &str = "did:plc:teq";
const BAILEY: &str = "did:plc:bailey";
const NEL_PDS: &str = "https://pds.nel.pet";
const OLAREN_PDS: &str = "https://pds.olaren.dev";

type Responder = Box<dyn Fn(&HttpRequest) -> Result<HttpResponse, NetworkError> + Send + Sync>;

fn responding(status: http::StatusCode, body: bytes::Bytes) -> HttpResponse {
    HttpResponse {
        status,
        headers: http::HeaderMap::new(),
        body,
    }
}

struct Hosted {
    owner: OwnerDid,
    repo: RepoDid,
    name: RepoName,
}

fn account(did: &str) -> AccountDid {
    AccountDid::new(did).unwrap()
}

fn anemone() -> Hosted {
    Hosted {
        owner: OwnerDid::new(NEL).unwrap(),
        repo: RepoDid::new("did:plc:squid").unwrap(),
        name: RepoName::new("anemone").unwrap(),
    }
}

fn barnacle() -> Hosted {
    Hosted {
        owner: OwnerDid::new(OLAREN).unwrap(),
        repo: RepoDid::new("did:plc:limpet").unwrap(),
        name: RepoName::new("barnacle").unwrap(),
    }
}

fn hosted_index(scratch: &TempDir, repos: &[Hosted], members: &[AccountDid]) -> Arc<Index> {
    let meta_path = scratch.path().join("meta");
    Repo::create(&meta_path).unwrap();
    let layout = Layout::new(scratch.path().join("repos"));
    let meta = Repo::open(&meta_path).unwrap();
    let store = CobStore::new(&meta);
    let home = CobHome::from(&KnotId::new(KNOT_DID).unwrap());
    let signer = K256Signer::generate(&SeededEntropy::new(3));
    let registration = |hosted: &Hosted| {
        layout.create(&hosted.repo).unwrap();
        RegistryChange::Register(Registration {
            owner: hosted.owner.clone(),
            rkey: RepoRkey::new(hosted.name.as_str()).unwrap(),
            name: hosted.name.clone(),
            repo: hosted.repo.clone(),
            created_at: UnixSeconds::new(1),
        })
    };
    let (first, rest) = repos.split_first().expect("a knot under test hosts a repo");
    let registry = store
        .create(&home, &registration(first), &signer, UnixSeconds::new(1))
        .unwrap()
        .object;
    rest.iter().for_each(|hosted| {
        store
            .update(
                &home,
                registry,
                &registration(hosted),
                &signer,
                UnixSeconds::new(2),
            )
            .unwrap();
    });

    let granted = |subject: &AccountDid| {
        MembersChange::Add(Grant {
            subject: subject.clone(),
            added_by: account(NEL),
            created_at: UnixSeconds::new(1),
        })
    };
    if let Some((first, rest)) = members.split_first() {
        let roll = store
            .create(&home, &granted(first), &signer, UnixSeconds::new(1))
            .unwrap()
            .object;
        rest.iter().for_each(|subject| {
            store
                .update(&home, roll, &granted(subject), &signer, UnixSeconds::new(2))
                .unwrap();
        });
    }

    let index = Arc::new(Index::new(meta_path, layout));
    index.rebuild().unwrap();
    index.warm_collaborators();
    index
}

fn atproto_with(responder: Responder) -> Arc<Atproto<FakeHttp<Responder>, ManualClock>> {
    Arc::new(Atproto::new(
        FakeHttp::new(responder),
        ManualClock::new(UnixMicros::new(1_000_000_000)),
        KnotId::new(KNOT_DID).unwrap(),
        knot_atproto::PlcDirectory::new(Url::parse("https://plc.directory/").unwrap()).unwrap(),
    ))
}

fn atproto_answering(
    status: http::StatusCode,
    calls: Arc<AtomicUsize>,
) -> Arc<Atproto<FakeHttp<Responder>, ManualClock>> {
    atproto_with(Box::new(move |_request: &HttpRequest| {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(responding(status, bytes::Bytes::new()))
    }))
}

fn did_document(did: &str, handle: &str, pds: &str) -> bytes::Bytes {
    let signing = K256Signer::generate(&SeededEntropy::new(5));
    let multibase = knot_types::crypto::multikey(0xe7, signing.public_key().as_bytes());
    let body = serde_json::json!({
        "id": did,
        "alsoKnownAs": [format!("at://{handle}")],
        "verificationMethod": [{
            "id": format!("{did}#atproto"),
            "type": "Multikey",
            "controller": did,
            "publicKeyMultibase": multibase,
        }],
        "service": [{
            "id": "#atproto_pds",
            "type": "AtprotoPersonalDataServer",
            "serviceEndpoint": pds,
        }]
    });
    bytes::Bytes::from(serde_json::to_vec(&body).unwrap())
}

fn atproto_serving_two_accounts() -> Arc<Atproto<FakeHttp<Responder>, ManualClock>> {
    atproto_with(Box::new(move |request: &HttpRequest| {
        let url = request.url.as_str();
        let body = if url.contains("listRecords") {
            bytes::Bytes::from_static(br#"{"records":[]}"#)
        } else if url.ends_with(NEL) {
            did_document(NEL, "nel.pet", NEL_PDS)
        } else if url.ends_with(OLAREN) {
            did_document(OLAREN, "olaren.dev", OLAREN_PDS)
        } else {
            return Ok(responding(http::StatusCode::NOT_FOUND, bytes::Bytes::new()));
        };
        Ok(responding(http::StatusCode::OK, body))
    }))
}

fn atproto_recording(
    seen: Arc<Mutex<Vec<String>>>,
) -> Arc<Atproto<FakeHttp<Responder>, ManualClock>> {
    atproto_with(Box::new(move |request: &HttpRequest| {
        let url = request.url.as_str();
        if url.contains("listRecords") {
            return Ok(responding(
                http::StatusCode::OK,
                bytes::Bytes::from_static(br#"{"records":[]}"#),
            ));
        }
        if url.ends_with(NEL) {
            return Ok(responding(
                http::StatusCode::OK,
                did_document(NEL, "nel.pet", NEL_PDS),
            ));
        }
        if let Some(did) = url.rsplit('/').next() {
            seen.lock().unwrap().push(did.to_string());
        }
        Ok(responding(
            http::StatusCode::SERVICE_UNAVAILABLE,
            bytes::Bytes::new(),
        ))
    }))
}

fn now() -> UnixSeconds {
    UnixSeconds::new(1_000)
}

fn brisk() -> Pace {
    Pace {
        busy: BusyRetry::from_millis(0),
        floor: SettleFloor::from_millis(0),
        ttl: KeyTtl::from_secs(3_600),
        reprieve: KeyReprieve::from_secs(300, 21_600),
        sweep: SweepFloor::DEFAULT,
        stalled: StalledBackoff::from_secs(1),
        settled: SettledPause::from_secs(1),
        members: AccountBudget::new(64),
        suspected: AccountBudget::new(256),
        host: RateLimit {
            burst: Burst::new(1),
            refill: RefillMicros::new(1_000),
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gone_document_completes_the_set_and_an_outage_leaves_it_warming() {
    let outcome = |status: http::StatusCode| async move {
        let scratch = tempfile::tempdir().unwrap();
        let index = hosted_index(&scratch, &[anemone()], &[]);
        let calls = Arc::new(AtomicUsize::new(0));
        let atproto = atproto_answering(status, Arc::clone(&calls));
        let pacer = HostPacer::new(brisk().host);
        assert_eq!(index.keys().coverage(), Coverage::Warming);
        fill_once(
            &index,
            &atproto,
            &Slots::testing(4),
            &pacer,
            brisk(),
            &Cursors::default(),
        )
        .await;
        assert!(
            calls.load(Ordering::SeqCst) > 0,
            "the fill made an outbound request"
        );
        index.keys().coverage()
    };

    assert_eq!(
        outcome(http::StatusCode::NOT_FOUND).await,
        Coverage::Ready,
        "a permanently unresolvable owner is recorded with an empty key set, so the set is complete"
    );
    assert_eq!(
        outcome(http::StatusCode::SERVICE_UNAVAILABLE).await,
        Coverage::Warming,
        "a transient failure doesn't teach the knot anything, so it mustn't claim a complete set"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keys_the_knot_already_read_survive_an_outage_and_a_refused_listing() {
    let scratch = tempfile::tempdir().unwrap();
    let index = hosted_index(&scratch, &[anemone()], &[]);
    let key = OfferedKey::from_bytes(vec![9]);
    let spent = KeyTtl::from_secs(1).lease_from(UnixSeconds::new(100));
    index.keys().record(&account(NEL), vec![key.clone()], spent);
    let pacer = HostPacer::new(brisk().host);

    let unreachable = atproto_answering(http::StatusCode::SERVICE_UNAVAILABLE, Arc::default());
    fill_once(
        &index,
        &unreachable,
        &Slots::testing(4),
        &pacer,
        brisk(),
        &Cursors::default(),
    )
    .await;
    assert_eq!(
        index.keys().coverage(),
        Coverage::Ready,
        "an owner the knot has read before keeps its last keys through an outage, so one \
         unreachable PDS mustn't reopen the knot to every offered key"
    );
    assert_eq!(
        index.owner_of_key(&key, now()),
        Resolved::Ready(Some(account(NEL))),
        "the reprieve keeps the keys the knot last read"
    );

    index.keys().record(&account(NEL), vec![key.clone()], spent);
    let listing = Arc::new(AtomicUsize::new(0));
    let refusing = {
        let listing = Arc::clone(&listing);
        atproto_with(Box::new(move |request: &HttpRequest| {
            let url = request.url.as_str();
            if url.contains("listRecords") {
                listing.fetch_add(1, Ordering::SeqCst);
                return Ok(responding(
                    http::StatusCode::BAD_REQUEST,
                    bytes::Bytes::new(),
                ));
            }
            Ok(responding(
                http::StatusCode::OK,
                did_document(NEL, "nel.pet", NEL_PDS),
            ))
        }))
    };
    fill_once(
        &index,
        &refusing,
        &Slots::testing(4),
        &pacer,
        brisk(),
        &Cursors::default(),
    )
    .await;
    assert!(
        listing.load(Ordering::SeqCst) > 0,
        "the fill read from the PDS"
    );
    assert_eq!(
        index.owner_of_key(&key, now()),
        Resolved::Ready(Some(account(NEL))),
        "an account whose DID document resolves hasn't gone anywhere, so the knot mustn't read \
         a 400 from a record listing as proof the account stopped publishing keys"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fill_takes_a_turn_at_each_host_it_reads_from_and_stops_on_shutdown() {
    let scratch = tempfile::tempdir().unwrap();
    let index = hosted_index(&scratch, &[anemone(), barnacle()], &[]);
    let atproto = atproto_serving_two_accounts();
    let pacer = HostPacer::new(brisk().host);

    fill_once(
        &index,
        &atproto,
        &Slots::testing(4),
        &pacer,
        brisk(),
        &Cursors::default(),
    )
    .await;

    assert_eq!(
        index.keys().coverage(),
        Coverage::Ready,
        "both owners resolved, so every account that may push has a record"
    );
    ["plc.directory", "pds.nel.pet", "pds.olaren.dev"]
        .iter()
        .for_each(|host| {
            assert!(
                !pacer.reserve_now(&HostKey::new(host), UnixMicros::new(0)),
                "the fill must take a turn at {host} before it reads from it, \
                 or a knot whose members share one PDS spends its whole rate at that host"
            );
        });
    assert!(
        pacer.reserve_now(&HostKey::new("pds.teq.dev"), UnixMicros::new(0)),
        "a host the fill never read from is due immediately. The bookings above are the fill's \
         own work"
    );

    let shutdown = CancellationToken::new();
    let task = knot_keyfill::spawn(
        Arc::clone(&index),
        Arc::clone(&atproto),
        Slots::testing(4),
        brisk(),
        shutdown.clone(),
    );
    shutdown.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("a shutting-down knot mustn't wait out the pause between fill passes")
        .expect("the fill task stops without panicking");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn members_are_read_whole_on_first_contact_then_renewed_one_budget_turn_per_pass() {
    let scratch = tempfile::tempdir().unwrap();
    let members = [account(OLAREN), account(TEQ), account(BAILEY)];
    let index = hosted_index(&scratch, &[anemone()], &members);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let atproto = atproto_recording(Arc::clone(&seen));
    let pacer = HostPacer::new(brisk().host);
    let pace = Pace {
        members: AccountBudget::new(1),
        ..brisk()
    };
    let cursor = Cursors::default();

    fill_once(&index, &atproto, &Slots::testing(4), &pacer, pace, &cursor).await;
    assert_eq!(
        index.keys().coverage(),
        Coverage::Ready,
        "the pushers are what coverage waits on, so an unreadable member mustn't make the knot \
         doubt the keys it checks pushes against"
    );
    let mut attempted = seen.lock().unwrap().clone();
    attempted.sort();
    assert_eq!(
        attempted,
        vec![BAILEY.to_string(), OLAREN.to_string(), TEQ.to_string()],
        "the renewal budget paces rereads, so a member the knot has never read mustn't wait \
         its turn behind it and be refused at the handshake for the passes in between"
    );

    let spent = KeyTtl::from_secs(1).lease_from(UnixSeconds::new(0));
    members
        .iter()
        .for_each(|member| _ = index.keys().record(member, Vec::new(), spent));
    seen.lock().unwrap().clear();

    futures::stream::iter(0..3)
        .for_each(|_| async {
            fill_once(&index, &atproto, &Slots::testing(4), &pacer, pace, &cursor).await;
        })
        .await;
    let order = seen.lock().unwrap().clone();
    assert_eq!(
        order.iter().map(String::as_str).collect::<Vec<_>>(),
        vec![BAILEY, OLAREN, TEQ],
        "one member per pass in turn, or members whose PDS stays down keep the front of \
         the queue and the knot never reads the rest"
    );
}
