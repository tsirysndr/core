use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use knot_atproto::Atproto;
use knot_index::{Index, IndexGeneration, Resolved};
use knot_runtime::{Clock, HttpTransport, UnixMicros};
use knot_types::{AccountDid, OfferedKey};

const FRESH_TTL: Duration = Duration::from_secs(60);
const DEGRADED_TTL: Duration = Duration::from_secs(5);
const BACKOFF_SHIFT_LIMIT: u32 = 4;
const MISS_REVALIDATE_BUDGET: Duration = Duration::from_secs(30);
const RESOLVE_FANOUT: usize = 16;

fn degraded_ttl(consecutive_failures: u32) -> Duration {
    let secs = DEGRADED_TTL
        .as_secs()
        .saturating_mul(1u64 << consecutive_failures.min(BACKOFF_SHIFT_LIMIT))
        .min(FRESH_TTL.as_secs());
    Duration::from_secs(secs)
}

struct Freshness {
    due: UnixMicros,
    generation: IndexGeneration,
}

#[derive(Debug, PartialEq, Eq)]
enum Staleness {
    Fresh,
    Revalidate,
    Cold,
}

pub(crate) struct KeyRoster {
    by_did: Mutex<HashMap<AccountDid, HashSet<OfferedKey>>>,
    recognized: Mutex<HashSet<OfferedKey>>,
    freshness: Mutex<Option<Freshness>>,
    failures: AtomicU32,
    refresh: tokio::sync::Mutex<()>,
    refresh_in_flight: AtomicBool,
}

impl KeyRoster {
    pub(crate) fn new() -> Self {
        Self {
            by_did: Mutex::new(HashMap::new()),
            recognized: Mutex::new(HashSet::new()),
            freshness: Mutex::new(None),
            failures: AtomicU32::new(0),
            refresh: tokio::sync::Mutex::new(()),
            refresh_in_flight: AtomicBool::new(false),
        }
    }

    pub(crate) fn recognizes(&self, key: &OfferedKey) -> bool {
        self.recognized
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(key)
    }

    pub(crate) fn did_for(&self, key: &OfferedKey) -> Option<AccountDid> {
        self.by_did
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|(_, keys)| keys.contains(key))
            .map(|(did, _)| did.clone())
    }

    fn is_fresh(&self, now: UnixMicros, generation: IndexGeneration) -> bool {
        self.freshness
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .is_some_and(|fresh| now.get() < fresh.due.get() && fresh.generation == generation)
    }

    pub(crate) fn prime<H: HttpTransport, C: Clock>(
        self: &Arc<Self>,
        index: &Arc<Index>,
        atproto: &Arc<Atproto<H, C>>,
    ) {
        self.spawn_refresh(index, atproto);
    }

    pub(crate) fn ensure_fresh<H: HttpTransport, C: Clock>(
        self: &Arc<Self>,
        index: &Arc<Index>,
        atproto: &Arc<Atproto<H, C>>,
    ) {
        match self.staleness(atproto.now(), index.generation()) {
            Staleness::Fresh => {}
            Staleness::Revalidate | Staleness::Cold => self.spawn_refresh(index, atproto),
        }
    }

    pub(crate) async fn recognizes_fresh<H: HttpTransport, C: Clock>(
        self: &Arc<Self>,
        key: &OfferedKey,
        index: &Arc<Index>,
        atproto: &Arc<Atproto<H, C>>,
    ) -> bool {
        if self.recognizes(key) {
            self.ensure_fresh(index, atproto);
            return true;
        }
        if self.is_fresh(atproto.now(), index.generation()) {
            return false;
        }
        let _ = tokio::time::timeout(MISS_REVALIDATE_BUDGET, self.refresh(index, atproto)).await;
        self.recognizes(key)
    }

    fn staleness(&self, now: UnixMicros, generation: IndexGeneration) -> Staleness {
        match self
            .freshness
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            None => Staleness::Cold,
            Some(fresh) if fresh.generation != generation => Staleness::Revalidate,
            Some(fresh) if now.get() < fresh.due.get() => Staleness::Fresh,
            Some(_) => Staleness::Revalidate,
        }
    }

    fn spawn_refresh<H: HttpTransport, C: Clock>(
        self: &Arc<Self>,
        index: &Arc<Index>,
        atproto: &Arc<Atproto<H, C>>,
    ) {
        if self
            .refresh_in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let roster = Arc::clone(self);
        let index = Arc::clone(index);
        let atproto = Arc::clone(atproto);
        tokio::spawn(async move {
            let _in_flight = InFlightGuard(&roster.refresh_in_flight);
            roster.refresh(&index, &atproto).await;
        });
    }

    async fn refresh<H: HttpTransport, C: Clock>(&self, index: &Index, atproto: &Atproto<H, C>) {
        let _single_flight = self.refresh.lock().await;
        if self.is_fresh(atproto.now(), index.generation()) {
            return;
        }
        let generation = index.generation();
        let (dids, incomplete) = relevant_dids(index);
        let resolved: Vec<(AccountDid, Option<Vec<OfferedKey>>)> = futures::stream::iter(dids)
            .map(|did| async move {
                let keys = atproto.resolve_pubkeys(&did).await.ok();
                (did, keys)
            })
            .buffer_unordered(RESOLVE_FANOUT)
            .collect()
            .await;
        let any_failed = resolved.iter().any(|(_, keys)| keys.is_none());
        let relevant: HashSet<AccountDid> = resolved.iter().map(|(did, _)| did.clone()).collect();
        {
            let mut by_did = self
                .by_did
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            by_did.retain(|did, _| relevant.contains(did));
            resolved.into_iter().for_each(|(did, keys)| {
                if let Some(keys) = keys {
                    by_did.insert(did, keys.into_iter().collect());
                }
            });
            let union: HashSet<OfferedKey> = by_did.values().flatten().cloned().collect();
            *self
                .recognized
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = union;
        }
        let ttl = if any_failed {
            degraded_ttl(self.failures.fetch_add(1, Ordering::Relaxed))
        } else {
            self.failures.store(0, Ordering::Relaxed);
            if incomplete { DEGRADED_TTL } else { FRESH_TTL }
        };
        let due = UnixMicros::new(atproto.now().get().saturating_add(ttl.as_micros() as u64));
        *self
            .freshness
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Freshness { due, generation });
    }
}

struct InFlightGuard<'a>(&'a AtomicBool);

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn relevant_dids(index: &Index) -> (Vec<AccountDid>, bool) {
    let (mut dids, incomplete): (Vec<AccountDid>, bool) = index
        .hosted_repos()
        .iter()
        .map(|repo| {
            let (owner, owner_warming) = match index.owner_of(repo) {
                Resolved::Ready(Some(owner)) => (Some(AccountDid::from(owner)), false),
                Resolved::Ready(None) => (None, false),
                Resolved::Warming => (None, true),
            };
            let (collaborators, collaborators_warming) = match index.collaborators_of(repo) {
                Resolved::Ready(collaborators) => (collaborators, false),
                Resolved::Warming => (Vec::new(), true),
            };
            (
                owner.into_iter().chain(collaborators).collect::<Vec<_>>(),
                owner_warming || collaborators_warming,
            )
        })
        .fold(
            (Vec::new(), false),
            |(mut acc, warming), (dids, repo_warming)| {
                acc.extend(dids);
                (acc, warming || repo_warming)
            },
        );
    dids.sort();
    dids.dedup();
    (dids, incomplete)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use knot_atproto::Atproto;
    use knot_cob::{CobHome, CobStore};
    use knot_cobs::{Registration, RegistryChange};
    use knot_git::{Layout, Repo};
    use knot_runtime::{
        FakeHttp, HttpRequest, HttpResponse, K256Signer, NetworkError, SeededEntropy, Signer,
    };
    use knot_types::{KnotId, OwnerDid, RepoDid, RepoName, RepoRkey, UnixSeconds, crypto};
    use russh::keys::{Algorithm, PrivateKey};
    use url::Url;

    struct SharedClock(Arc<AtomicU64>);
    impl Clock for SharedClock {
        fn now_unix_micros(&self) -> UnixMicros {
            UnixMicros::new(self.0.load(Ordering::SeqCst))
        }
    }

    fn line_and_offered() -> (String, OfferedKey) {
        let key = PrivateKey::random(&mut crate::EntropyRng, Algorithm::Ed25519).unwrap();
        let public = key.public_key();
        (
            public.to_openssh().unwrap(),
            OfferedKey::from_bytes(public.to_bytes().unwrap()),
        )
    }

    type Responder = Box<dyn Fn(&HttpRequest) -> Result<HttpResponse, NetworkError> + Send + Sync>;

    struct Harness {
        index: Arc<Index>,
        atproto: Arc<Atproto<FakeHttp<Responder>, SharedClock>>,
        published: Arc<std::sync::Mutex<Vec<String>>>,
        list_calls: Arc<AtomicUsize>,
        _dir: tempfile::TempDir,
    }

    fn harness(initial: Vec<String>) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("meta");
        Repo::create(&meta_path).unwrap();
        let layout = Layout::new(dir.path().join("repos"));
        let repo_did = RepoDid::new("did:plc:squid").unwrap();
        layout.create(&repo_did).unwrap();
        let cob_signer = K256Signer::generate(&SeededEntropy::new(2));
        {
            let meta = Repo::open(&meta_path).unwrap();
            CobStore::new(&meta)
                .create(
                    &CobHome::from(&KnotId::new("did:web:nel.pet").unwrap()),
                    &RegistryChange::Register(Registration {
                        owner: OwnerDid::new("did:plc:nel").unwrap(),
                        rkey: RepoRkey::new("anemone").unwrap(),
                        name: RepoName::new("anemone").unwrap(),
                        repo: repo_did.clone(),
                        created_at: UnixSeconds::new(1),
                    }),
                    &cob_signer,
                    UnixSeconds::new(1),
                )
                .unwrap();
        }
        let index = Arc::new(Index::new(meta_path, layout.clone()));
        index.rebuild().unwrap();

        let published = Arc::new(std::sync::Mutex::new(initial));
        let list_calls = Arc::new(AtomicUsize::new(0));
        let multikey = crypto::multikey(
            0xe7,
            K256Signer::generate(&SeededEntropy::new(7))
                .public_key()
                .as_bytes(),
        );
        let clock = Arc::new(AtomicU64::new(1_000_000_000));

        let responder: Responder = {
            let published = Arc::clone(&published);
            let list_calls = Arc::clone(&list_calls);
            Box::new(move |request: &HttpRequest| {
                let host = request.url.host_str().unwrap_or_default().to_string();
                let body = if host == "pds.oyster.cafe" {
                    list_calls.fetch_add(1, Ordering::SeqCst);
                    let records: Vec<_> = published
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|line| {
                            serde_json::json!({
                                "uri": "at://did:plc:nel/sh.tangled.publicKey/1",
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
                } else if host == "plc.directory" {
                    serde_json::to_vec(&serde_json::json!({
                        "id": "did:plc:nel",
                        "alsoKnownAs": ["at://nel.pet"],
                        "verificationMethod": [{
                            "id": "did:plc:nel#atproto",
                            "type": "Multikey",
                            "controller": "did:plc:nel",
                            "publicKeyMultibase": multikey
                        }],
                        "service": [{
                            "id": "#atproto_pds",
                            "type": "AtprotoPersonalDataServer",
                            "serviceEndpoint": "https://pds.oyster.cafe"
                        }]
                    }))
                    .unwrap()
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
        };

        let atproto = Arc::new(Atproto::new(
            FakeHttp::new(responder),
            SharedClock(clock),
            KnotId::new("did:web:nel.pet").unwrap(),
            knot_atproto::PlcDirectory::new(Url::parse("https://plc.directory/").unwrap()).unwrap(),
        ));

        Harness {
            index,
            atproto,
            published,
            list_calls,
            _dir: dir,
        }
    }

    async fn wait_recognized(roster: &KeyRoster, key: &OfferedKey) {
        for _ in 0..1000 {
            if roster.recognizes(key) {
                return;
            }
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn an_acl_write_makes_a_freshly_published_key_recognized_without_waiting_for_the_ttl() {
        let (line1, offered1) = line_and_offered();
        let (line2, offered2) = line_and_offered();

        let Harness {
            index,
            atproto,
            published,
            list_calls,
            _dir,
        } = harness(vec![line1]);

        let roster = Arc::new(KeyRoster::new());
        roster.ensure_fresh(&index, &atproto);
        wait_recognized(&roster, &offered1).await;
        assert!(roster.recognizes(&offered1));
        assert_eq!(list_calls.load(Ordering::SeqCst), 1);

        published.lock().unwrap().push(line2.clone());

        roster.ensure_fresh(&index, &atproto);
        assert!(
            !roster.recognizes(&offered2),
            "stable index and unexpired TTL still serves cached roster, no re-resolution"
        );
        assert_eq!(list_calls.load(Ordering::SeqCst), 1);

        index.refresh_members().unwrap();
        roster.ensure_fresh(&index, &atproto);
        wait_recognized(&roster, &offered2).await;
        assert!(
            roster.recognizes(&offered2),
            "ACL write bumps generation, so roster revalidates off the auth path"
        );
        assert_eq!(
            list_calls.load(Ordering::SeqCst),
            2,
            "exactly one async re-resolution off the auth path"
        );
    }

    #[tokio::test]
    async fn a_miss_against_a_stale_roster_blocks_bounded_to_revalidate_before_rejecting() {
        let (line1, offered1) = line_and_offered();
        let (line2, offered2) = line_and_offered();
        let Harness {
            index,
            atproto,
            published,
            list_calls,
            _dir,
        } = harness(vec![line1]);
        let roster = Arc::new(KeyRoster::new());

        assert!(
            roster.recognizes_fresh(&offered1, &index, &atproto).await,
            "the first handshake blocks on the primed resolve and recognizes the published key"
        );
        assert_eq!(list_calls.load(Ordering::SeqCst), 1);

        published.lock().unwrap().push(line2.clone());
        index.refresh_members().unwrap();

        assert!(
            roster.recognizes_fresh(&offered2, &index, &atproto).await,
            "a generation-bumped miss blocks to revalidate and picks up the new key on the first attempt"
        );
        assert_eq!(
            list_calls.load(Ordering::SeqCst),
            2,
            "the miss triggers exactly one bounded re-resolution"
        );
    }

    #[test]
    fn staleness_classifies_cold_fresh_and_revalidate() {
        let roster = KeyRoster::new();
        assert_eq!(
            roster.staleness(UnixMicros::new(0), IndexGeneration::new(0)),
            Staleness::Cold,
            "with no roster yet the first auth is cold and must revalidate before it can answer a miss"
        );
        *roster
            .freshness
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Freshness {
            due: UnixMicros::new(1_000),
            generation: IndexGeneration::new(0),
        });
        assert_eq!(
            roster.staleness(UnixMicros::new(500), IndexGeneration::new(0)),
            Staleness::Fresh
        );
        assert_eq!(
            roster.staleness(UnixMicros::new(500), IndexGeneration::new(1)),
            Staleness::Revalidate,
            "an ACL write moves the generation, so the cached roster is stale"
        );
        assert_eq!(
            roster.staleness(UnixMicros::new(2_000), IndexGeneration::new(0)),
            Staleness::Revalidate,
            "an expired ttl at the same generation is stale too"
        );
    }

    #[test]
    fn degraded_ttl_backs_off_from_the_short_retry_to_the_fresh_ceiling() {
        assert_eq!(degraded_ttl(0), Duration::from_secs(5));
        assert_eq!(degraded_ttl(1), Duration::from_secs(10));
        assert_eq!(degraded_ttl(2), Duration::from_secs(20));
        assert_eq!(degraded_ttl(3), Duration::from_secs(40));
        assert_eq!(degraded_ttl(4), Duration::from_secs(60));
        assert_eq!(
            degraded_ttl(50),
            Duration::from_secs(60),
            "a persistently unresolvable did clamps the retry to the fresh ttl instead of storming"
        );
    }
}
