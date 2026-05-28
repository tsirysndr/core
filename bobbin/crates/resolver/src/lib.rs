mod legacy_upgrade;
mod normalize;

pub use legacy_upgrade::{
    DecodedRecord, decode_canon_or_upgrade, decode_canon_or_upgrade_bytes, normalize_record_fields,
    scrub_record_bytes, synthesize_created_at, upgrade, upgrade_wire_bytes,
};
pub use normalize::NormalizeRepoRefs;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::time::Instant;

use bobbin_runtime::{Clock, RuntimeHasher};
use bobbin_slingshot_client::{SlingshotClient, SlingshotError};
use bobbin_types::edges::{ExtractError, Record};
use bobbin_types::ids::{RepoIdent, nsid_static};
use jacquard_common::DefaultStr;
use jacquard_common::types::did::Did;
use jacquard_common::types::nsid::Nsid;
use jacquard_common::types::recordkey::Rkey;
use scc::HashMap as SccMap;
use tokio::sync::OnceCell;
use tracing::warn;

const REPO_COLLECTION: &str = "sh.tangled.repo";
const TRANSIENT_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Resolution {
    Mapped(Did<DefaultStr>),
    NoRepoDid,
    Unresolvable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AuthoritativeResolution {
    Mapped(Did<DefaultStr>),
    NoRepoDid,
}

impl AuthoritativeResolution {
    fn from_repo_did(repo_did: Option<Did<DefaultStr>>) -> Self {
        match repo_did {
            Some(did) => Self::Mapped(did),
            None => Self::NoRepoDid,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CacheEntry {
    Authoritative(AuthoritativeResolution),
    Provisional(Resolution),
    Transient { expires_at: Instant },
}

impl CacheEntry {
    fn into_resolution(self) -> Resolution {
        match self {
            Self::Authoritative(AuthoritativeResolution::Mapped(did)) => Resolution::Mapped(did),
            Self::Authoritative(AuthoritativeResolution::NoRepoDid) => Resolution::NoRepoDid,
            Self::Provisional(r) => r,
            Self::Transient { .. } => Resolution::Unresolvable,
        }
    }

    fn is_expired_transient(&self, now: Instant) -> bool {
        matches!(self, Self::Transient { expires_at } if *expires_at <= now)
    }
}

#[derive(Default)]
pub struct ResolverStats {
    hits: AtomicU64,
    misses_mapped: AtomicU64,
    misses_no_repo_did: AtomicU64,
    misses_unresolvable: AtomicU64,
    misses_transient: AtomicU64,
    misses_no_client: AtomicU64,
    miss_latency_micros_sum: AtomicU64,
    miss_latency_micros_max: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResolverStatsSnapshot {
    pub hits: u64,
    pub misses_mapped: u64,
    pub misses_no_repo_did: u64,
    pub misses_unresolvable: u64,
    pub misses_transient: u64,
    pub misses_no_client: u64,
    pub miss_latency_micros_sum: u64,
    pub miss_latency_micros_max: u64,
}

impl ResolverStatsSnapshot {
    pub fn miss_count(&self) -> u64 {
        self.misses_mapped
            + self.misses_no_repo_did
            + self.misses_unresolvable
            + self.misses_transient
            + self.misses_no_client
    }

    pub fn total(&self) -> u64 {
        self.hits + self.miss_count()
    }

    pub fn miss_latency_micros_avg(&self) -> Option<u64> {
        let misses = self.miss_count() - self.misses_no_client;
        (misses > 0).then(|| self.miss_latency_micros_sum / misses)
    }
}

#[derive(Clone, Copy)]
enum MissKind {
    Mapped,
    NoRepoDid,
    Unresolvable,
    Transient,
    NoClient,
}

impl ResolverStats {
    fn record_hit(&self) {
        self.hits.fetch_add(1, Ordering::Relaxed);
    }

    fn record_miss(&self, kind: MissKind, latency: Option<Duration>) {
        let counter = match kind {
            MissKind::Mapped => &self.misses_mapped,
            MissKind::NoRepoDid => &self.misses_no_repo_did,
            MissKind::Unresolvable => &self.misses_unresolvable,
            MissKind::Transient => &self.misses_transient,
            MissKind::NoClient => &self.misses_no_client,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        if let Some(latency) = latency {
            let micros = u64::try_from(latency.as_micros()).unwrap_or(u64::MAX);
            self.miss_latency_micros_sum
                .fetch_add(micros, Ordering::Relaxed);
            self.miss_latency_micros_max
                .fetch_max(micros, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> ResolverStatsSnapshot {
        ResolverStatsSnapshot {
            hits: self.hits.load(Ordering::Relaxed),
            misses_mapped: self.misses_mapped.load(Ordering::Relaxed),
            misses_no_repo_did: self.misses_no_repo_did.load(Ordering::Relaxed),
            misses_unresolvable: self.misses_unresolvable.load(Ordering::Relaxed),
            misses_transient: self.misses_transient.load(Ordering::Relaxed),
            misses_no_client: self.misses_no_client.load(Ordering::Relaxed),
            miss_latency_micros_sum: self.miss_latency_micros_sum.load(Ordering::Relaxed),
            miss_latency_micros_max: self.miss_latency_micros_max.load(Ordering::Relaxed),
        }
    }
}

struct SlingshotProbe {
    client: SlingshotClient,
    clock: Arc<dyn Clock>,
}

pub struct RepoIdResolver {
    cache: SccMap<RepoIdent, CacheEntry, RuntimeHasher>,
    by_repo_did: SccMap<Did<DefaultStr>, RepoIdent, RuntimeHasher>,
    in_flight: SccMap<RepoIdent, Arc<OnceCell<Resolution>>, RuntimeHasher>,
    probe: Option<SlingshotProbe>,
    stats: ResolverStats,
}

impl RepoIdResolver {
    pub fn with_slingshot(
        client: SlingshotClient,
        clock: Arc<dyn Clock>,
        hasher: RuntimeHasher,
    ) -> Self {
        Self {
            cache: SccMap::with_hasher(hasher.clone()),
            by_repo_did: SccMap::with_hasher(hasher.clone()),
            in_flight: SccMap::with_hasher(hasher),
            probe: Some(SlingshotProbe { client, clock }),
            stats: ResolverStats::default(),
        }
    }

    pub fn detached(hasher: RuntimeHasher) -> Self {
        Self {
            cache: SccMap::with_hasher(hasher.clone()),
            by_repo_did: SccMap::with_hasher(hasher.clone()),
            in_flight: SccMap::with_hasher(hasher),
            probe: None,
            stats: ResolverStats::default(),
        }
    }

    pub fn stats(&self) -> ResolverStatsSnapshot {
        self.stats.snapshot()
    }

    pub async fn cached_resolution(
        &self,
        owner: &Did<DefaultStr>,
        rkey: &Rkey<DefaultStr>,
    ) -> Option<Resolution> {
        let key = RepoIdent::new(owner.clone(), rkey.clone());
        let entry = self.cache.get_async(&key).await?;
        let now = self.probe.as_ref().map(|p| p.clock.now_instant());
        if let Some(now) = now
            && entry.get().is_expired_transient(now)
        {
            return None;
        }
        Some(entry.get().clone().into_resolution())
    }

    pub async fn lookup_by_repo_did(&self, repo_did: &Did<DefaultStr>) -> Option<RepoIdent> {
        self.by_repo_did
            .get_async(repo_did)
            .await
            .map(|e| e.get().clone())
    }

    pub async fn observe(
        &self,
        owner: Did<DefaultStr>,
        rkey: Rkey<DefaultStr>,
        repo_did: Option<Did<DefaultStr>>,
    ) -> Option<RepoIdent> {
        let ident = RepoIdent::new(owner, rkey);
        let entry =
            CacheEntry::Authoritative(AuthoritativeResolution::from_repo_did(repo_did.clone()));
        self.cache
            .entry_async(ident.clone())
            .await
            .and_modify(|existing| *existing = entry.clone())
            .or_insert(entry);

        let repo_did = repo_did?;
        let mut prior: Option<RepoIdent> = None;
        self.by_repo_did
            .entry_async(repo_did)
            .await
            .and_modify(|existing| {
                if *existing != ident {
                    prior = Some(existing.clone());
                    *existing = ident.clone();
                }
            })
            .or_insert(ident);
        prior
    }

    pub async fn forget(&self, owner: &Did<DefaultStr>, rkey: &Rkey<DefaultStr>) {
        let ident = RepoIdent::new(owner.clone(), rkey.clone());
        let prior_resolution = self
            .cache
            .remove_async(&ident)
            .await
            .map(|(_, entry)| entry.into_resolution());
        if let Some(Resolution::Mapped(repo_did)) = prior_resolution {
            self.by_repo_did
                .remove_if_async(&repo_did, |existing| *existing == ident)
                .await;
        }
    }

    async fn fill_provisional(&self, key: RepoIdent, resolution: Resolution) {
        let entry = CacheEntry::Provisional(resolution);
        self.cache
            .entry_async(key)
            .await
            .and_modify(|existing| {
                if matches!(existing, CacheEntry::Authoritative(_)) {
                    return;
                }
                *existing = entry.clone();
            })
            .or_insert(entry);
    }

    async fn fill_transient(&self, key: RepoIdent, expires_at: Instant) {
        let entry = CacheEntry::Transient { expires_at };
        self.cache
            .entry_async(key)
            .await
            .and_modify(|existing| {
                if matches!(existing, CacheEntry::Authoritative(_)) {
                    return;
                }
                *existing = entry.clone();
            })
            .or_insert(entry);
    }

    pub async fn resolve(&self, owner: &Did<DefaultStr>, rkey: &Rkey<DefaultStr>) -> Resolution {
        let key = RepoIdent::new(owner.clone(), rkey.clone());

        let Some(probe) = self.probe.as_ref() else {
            if let Some(entry) = self.cache.get_async(&key).await {
                self.stats.record_hit();
                return entry.get().clone().into_resolution();
            }
            self.stats.record_miss(MissKind::NoClient, None);
            return Resolution::Unresolvable;
        };

        let now = probe.clock.now_instant();
        if let Some(entry) = self.cache.get_async(&key).await
            && !entry.get().is_expired_transient(now)
        {
            self.stats.record_hit();
            return entry.get().clone().into_resolution();
        }

        let cell: Arc<OnceCell<Resolution>> = self
            .in_flight
            .entry_async(key.clone())
            .await
            .or_insert_with(|| Arc::new(OnceCell::new()))
            .get()
            .clone();

        let result = cell
            .get_or_init(|| async { self.fetch_repo_did(owner, rkey, &key).await })
            .await
            .clone();

        self.in_flight.remove_async(&key).await;

        result
    }

    async fn fetch_repo_did(
        &self,
        owner: &Did<DefaultStr>,
        rkey: &Rkey<DefaultStr>,
        key: &RepoIdent,
    ) -> Resolution {
        let probe = self
            .probe
            .as_ref()
            .expect("fetch_repo_did is only called when a probe is present");
        let started = probe.clock.now_instant();
        let nsid: Nsid<DefaultStr> = nsid_static(REPO_COLLECTION);
        let provisional = match probe.client.get_record(owner, &nsid, rkey).await {
            Ok(body) => match repo_did_from_body(&nsid, &body.value) {
                Ok(Some(did)) => Resolution::Mapped(did),
                Ok(None) => Resolution::NoRepoDid,
                Err(e) => {
                    warn!(
                        error = ?e,
                        owner = owner.as_ref(),
                        rkey = rkey.as_ref(),
                        "slingshot returned unparseable repo body, caching as unresolvable",
                    );
                    Resolution::Unresolvable
                }
            },
            Err(SlingshotError::NotFound) => {
                warn!(
                    owner = owner.as_ref(),
                    rkey = rkey.as_ref(),
                    "no repo record on slingshot, caching as unresolvable",
                );
                Resolution::Unresolvable
            }
            Err(ref e) if is_garbage_response(e) => {
                warn!(
                    error = ?e,
                    owner = owner.as_ref(),
                    rkey = rkey.as_ref(),
                    "slingshot returned malformed response, caching as unresolvable",
                );
                Resolution::Unresolvable
            }
            Err(e) => {
                warn!(
                    error = ?e,
                    owner = owner.as_ref(),
                    rkey = rkey.as_ref(),
                    "caching transient slingshot failure for repoDID lookup under short TTL",
                );
                let elapsed = probe.clock.now_instant().duration_since(started);
                self.stats.record_miss(MissKind::Transient, Some(elapsed));
                let expires_at = probe.clock.now_instant() + TRANSIENT_TTL;
                self.fill_transient(key.clone(), expires_at).await;
                return Resolution::Unresolvable;
            }
        };
        let elapsed = probe.clock.now_instant().duration_since(started);
        let kind = match &provisional {
            Resolution::Mapped(_) => MissKind::Mapped,
            Resolution::NoRepoDid => MissKind::NoRepoDid,
            Resolution::Unresolvable => MissKind::Unresolvable,
        };
        self.stats.record_miss(kind, Some(elapsed));
        self.fill_provisional(key.clone(), provisional.clone())
            .await;
        provisional
    }
}

fn is_garbage_response(err: &SlingshotError) -> bool {
    matches!(
        err,
        SlingshotError::Decode(_)
            | SlingshotError::MissingField(_)
            | SlingshotError::InvalidAtUri(_)
            | SlingshotError::InvalidCid(_)
            | SlingshotError::UriMismatch { .. },
    )
}

fn repo_did_from_body(
    nsid: &Nsid<DefaultStr>,
    body: &[u8],
) -> Result<Option<Did<DefaultStr>>, ExtractError> {
    match DecodedRecord::try_decode(nsid, body)? {
        DecodedRecord::Canon(Record::Repo(repo)) => Ok(repo.repo_did),
        DecodedRecord::Canon(_) | DecodedRecord::Legacy(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobbin_runtime::SystemClock;
    use jacquard_common::types::did::Did;
    use jacquard_common::types::recordkey::Rkey;

    fn did(s: &str) -> Did<DefaultStr> {
        Did::new_owned(s).unwrap()
    }

    fn rkey(s: &str) -> Rkey<DefaultStr> {
        Rkey::new_owned(s).unwrap()
    }

    fn test_clock() -> Arc<dyn Clock> {
        Arc::new(SystemClock::new())
    }

    #[tokio::test]
    async fn observation_returns_prior_ident_when_repo_did_moves() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let prior = resolver
            .observe(
                did("did:plc:nel"),
                rkey("3liuighjy2h22"),
                Some(did("did:plc:clam")),
            )
            .await;
        assert!(prior.is_none(), "first observation has no prior");

        let prior = resolver
            .observe(did("did:plc:nel"), rkey("core"), Some(did("did:plc:clam")))
            .await;
        assert_eq!(
            prior,
            Some(RepoIdent::new(did("did:plc:nel"), rkey("3liuighjy2h22"))),
            "same repoDID at a new (owner, rkey) returns the prior ident so callers can evict the stale at-uri",
        );

        let prior = resolver
            .observe(did("did:plc:nel"), rkey("core"), Some(did("did:plc:clam")))
            .await;
        assert!(prior.is_none(), "re-observing the same ident is a no-op");
    }

    #[tokio::test]
    async fn observation_without_repo_did_does_not_track_reverse() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let prior = resolver
            .observe(did("did:plc:nel"), rkey("abcabcabcabcz"), None)
            .await;
        assert!(prior.is_none());
    }

    #[tokio::test]
    async fn forget_clears_reverse_only_when_still_owned() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        resolver
            .observe(
                did("did:plc:nel"),
                rkey("3liuighjy2h22"),
                Some(did("did:plc:clam")),
            )
            .await;
        resolver
            .observe(did("did:plc:nel"), rkey("core"), Some(did("did:plc:clam")))
            .await;

        resolver
            .forget(&did("did:plc:nel"), &rkey("3liuighjy2h22"))
            .await;

        let prior = resolver
            .observe(
                did("did:plc:nel"),
                rkey("core-renamed"),
                Some(did("did:plc:clam")),
            )
            .await;
        assert_eq!(
            prior,
            Some(RepoIdent::new(did("did:plc:nel"), rkey("core"))),
            "stale at-uri's forget must not displace the live owner of did:plc:clam",
        );
    }

    #[tokio::test]
    async fn observation_with_repo_did_resolves_mapped() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        resolver
            .observe(
                did("did:plc:nel"),
                rkey("abcabcabcabcz"),
                Some(did("did:plc:clam")),
            )
            .await;
        let got = resolver
            .resolve(&did("did:plc:nel"), &rkey("abcabcabcabcz"))
            .await;
        assert_eq!(got, Resolution::Mapped(did("did:plc:clam")));
    }

    #[tokio::test]
    async fn observation_without_repo_did_resolves_no_repo_did() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        resolver
            .observe(did("did:plc:nel"), rkey("abcabcabcabcz"), None)
            .await;
        let got = resolver
            .resolve(&did("did:plc:nel"), &rkey("abcabcabcabcz"))
            .await;
        assert_eq!(
            got,
            Resolution::NoRepoDid,
            "observed but empty repoDID is a definitive answer not a lookup failure",
        );
    }

    #[tokio::test]
    async fn cache_miss_without_client_is_unresolvable() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let got = resolver
            .resolve(&did("did:plc:nel"), &rkey("abcabcabcabcz"))
            .await;
        assert_eq!(got, Resolution::Unresolvable);
    }

    #[tokio::test]
    async fn lookup_by_repo_did_finds_observed_ident() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        resolver
            .observe(
                did("did:plc:nel"),
                rkey("abcabcabcabcz"),
                Some(did("did:plc:limpet")),
            )
            .await;
        let got = resolver.lookup_by_repo_did(&did("did:plc:limpet")).await;
        assert_eq!(
            got,
            Some(RepoIdent::new(did("did:plc:nel"), rkey("abcabcabcabcz"))),
        );
    }

    #[tokio::test]
    async fn lookup_by_repo_did_misses_when_unobserved() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let got = resolver.lookup_by_repo_did(&did("did:plc:limpet")).await;
        assert_eq!(got, None);
    }

    #[tokio::test]
    async fn lookup_by_repo_did_misses_when_repo_did_was_none() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        resolver
            .observe(did("did:plc:nel"), rkey("abcabcabcabcz"), None)
            .await;
        let got = resolver.lookup_by_repo_did(&did("did:plc:limpet")).await;
        assert_eq!(got, None);
    }

    #[tokio::test]
    async fn lookup_by_repo_did_follows_move_to_new_ident() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        resolver
            .observe(
                did("did:plc:nel"),
                rkey("abcabcabcabcz"),
                Some(did("did:plc:limpet")),
            )
            .await;
        resolver
            .observe(
                did("did:plc:olaren"),
                rkey("xyzxyzxyzxyzx"),
                Some(did("did:plc:limpet")),
            )
            .await;
        let got = resolver.lookup_by_repo_did(&did("did:plc:limpet")).await;
        assert_eq!(
            got,
            Some(RepoIdent::new(did("did:plc:olaren"), rkey("xyzxyzxyzxyzx"))),
        );
    }

    #[tokio::test]
    async fn observation_overwrites_prior_value() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        resolver
            .observe(
                did("did:plc:nel"),
                rkey("abcabcabcabcz"),
                Some(did("did:plc:clam")),
            )
            .await;
        resolver
            .observe(
                did("did:plc:nel"),
                rkey("abcabcabcabcz"),
                Some(did("did:plc:uni")),
            )
            .await;
        let got = resolver
            .resolve(&did("did:plc:nel"), &rkey("abcabcabcabcz"))
            .await;
        assert_eq!(got, Resolution::Mapped(did("did:plc:uni")));
    }

    #[tokio::test]
    async fn fill_provisional_does_not_downgrade_authoritative_mapped() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let owner = did("did:plc:nel");
        let key = rkey("abcabcabcabcz");
        resolver
            .observe(owner.clone(), key.clone(), Some(did("did:plc:clam")))
            .await;
        resolver
            .fill_provisional(
                RepoIdent::new(owner.clone(), key.clone()),
                Resolution::Unresolvable,
            )
            .await;
        let got = resolver.resolve(&owner, &key).await;
        assert_eq!(
            got,
            Resolution::Mapped(did("did:plc:clam")),
            "firehose-observed mapping must outrank provisional slingshot info",
        );
    }

    #[tokio::test]
    async fn fill_provisional_does_not_downgrade_authoritative_no_repo_did() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let owner = did("did:plc:nel");
        let key = rkey("abcabcabcabcz");
        resolver.observe(owner.clone(), key.clone(), None).await;
        resolver
            .fill_provisional(
                RepoIdent::new(owner.clone(), key.clone()),
                Resolution::Mapped(did("did:plc:clam")),
            )
            .await;
        let got = resolver.resolve(&owner, &key).await;
        assert_eq!(
            got,
            Resolution::NoRepoDid,
            "an authoritative empty observation must outrank provisional slingshot info even when slingshot disagrees",
        );
    }

    #[tokio::test]
    async fn slingshot_404_caches_as_unresolvable() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/xrpc/com.atproto.repo.getRecord"))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        let client =
            SlingshotClient::with_default_http(url::Url::parse(&server.uri()).unwrap()).unwrap();
        let resolver =
            RepoIdResolver::with_slingshot(client, test_clock(), RuntimeHasher::default());

        let owner = did("did:plc:nel");
        let key = rkey("abcabcabcabcz");
        let first = resolver.resolve(&owner, &key).await;
        let second = resolver.resolve(&owner, &key).await;
        assert_eq!(first, Resolution::Unresolvable);
        assert_eq!(second, Resolution::Unresolvable);
    }

    #[tokio::test]
    async fn slingshot_malformed_envelope_caches_as_unresolvable() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/xrpc/com.atproto.repo.getRecord"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string("not json"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client =
            SlingshotClient::with_default_http(url::Url::parse(&server.uri()).unwrap()).unwrap();
        let resolver =
            RepoIdResolver::with_slingshot(client, test_clock(), RuntimeHasher::default());

        let owner = did("did:plc:nel");
        let key = rkey("abcabcabcabcz");
        let first = resolver.resolve(&owner, &key).await;
        let second = resolver.resolve(&owner, &key).await;
        assert_eq!(first, Resolution::Unresolvable);
        assert_eq!(
            second,
            Resolution::Unresolvable,
            "garbage envelopes are stable across retries, so caching avoids hammering slingshot",
        );
    }

    #[tokio::test]
    async fn slingshot_uri_mismatch_caches_as_unresolvable() {
        let server = wiremock::MockServer::start().await;
        let body = serde_json::json!({
            "uri": "at://did:plc:limpet/sh.tangled.repo/elsewhere",
            "cid": "bafyreieqygohnz2zqyvtvktbjpvhutphobcmbsnt4q5lc36ri7vpcmoz4i",
            "value": {"$type": "sh.tangled.repo", "knot": "oyster.cafe", "createdAt": "2026-05-01T00:00:00Z"}
        });
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/xrpc/com.atproto.repo.getRecord"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;

        let client =
            SlingshotClient::with_default_http(url::Url::parse(&server.uri()).unwrap()).unwrap();
        let resolver =
            RepoIdResolver::with_slingshot(client, test_clock(), RuntimeHasher::default());

        let owner = did("did:plc:nel");
        let key = rkey("abcabcabcabcz");
        let first = resolver.resolve(&owner, &key).await;
        let second = resolver.resolve(&owner, &key).await;
        assert_eq!(first, Resolution::Unresolvable);
        assert_eq!(second, Resolution::Unresolvable);
    }

    #[tokio::test]
    async fn slingshot_legacy_repo_body_resolves_no_repo_did_not_unresolvable() {
        let server = wiremock::MockServer::start().await;
        let body = serde_json::json!({
            "uri": "at://did:plc:nel/sh.tangled.repo/abcabcabcabcz",
            "cid": "bafyreieqygohnz2zqyvtvktbjpvhutphobcmbsnt4q5lc36ri7vpcmoz4i",
            "value": {
                "$type": "sh.tangled.repo",
                "addedAt": "2025-03-07T21:47:53Z",
                "knot": "knot1.tangled.sh",
                "name": "scallop",
                "owner": "did:plc:nel",
            },
        });
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/xrpc/com.atproto.repo.getRecord"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;

        let client =
            SlingshotClient::with_default_http(url::Url::parse(&server.uri()).unwrap()).unwrap();
        let resolver =
            RepoIdResolver::with_slingshot(client, test_clock(), RuntimeHasher::default());

        let owner = did("did:plc:nel");
        let key = rkey("abcabcabcabcz");
        let got = resolver.resolve(&owner, &key).await;
        assert_eq!(
            got,
            Resolution::NoRepoDid,
            "legacy repo wires without a repo_did parse via legacy upgrade and resolve as NoRepoDid, not Unresolvable",
        );
    }

    #[tokio::test]
    async fn slingshot_unparseable_repo_value_caches_as_unresolvable() {
        let server = wiremock::MockServer::start().await;
        let body = serde_json::json!({
            "uri": "at://did:plc:nel/sh.tangled.repo/abcabcabcabcz",
            "cid": "bafyreieqygohnz2zqyvtvktbjpvhutphobcmbsnt4q5lc36ri7vpcmoz4i",
            "value": {"$type": "sh.tangled.repo"}
        });
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/xrpc/com.atproto.repo.getRecord"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;

        let client =
            SlingshotClient::with_default_http(url::Url::parse(&server.uri()).unwrap()).unwrap();
        let resolver =
            RepoIdResolver::with_slingshot(client, test_clock(), RuntimeHasher::default());

        let owner = did("did:plc:nel");
        let key = rkey("abcabcabcabcz");
        let first = resolver.resolve(&owner, &key).await;
        let second = resolver.resolve(&owner, &key).await;
        assert_eq!(
            first,
            Resolution::Unresolvable,
            "a repo body that fails lexicon validation must not be conflated with NoRepoDid",
        );
        assert_eq!(second, Resolution::Unresolvable);
    }

    #[tokio::test]
    async fn slingshot_transport_error_caches_with_short_ttl() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/xrpc/com.atproto.repo.getRecord"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;

        let client =
            SlingshotClient::with_default_http(url::Url::parse(&server.uri()).unwrap()).unwrap();
        let resolver =
            RepoIdResolver::with_slingshot(client, test_clock(), RuntimeHasher::default());

        let owner = did("did:plc:nel");
        let key = rkey("abcabcabcabcz");
        let first = resolver.resolve(&owner, &key).await;
        let second = resolver.resolve(&owner, &key).await;
        assert_eq!(first, Resolution::Unresolvable);
        assert_eq!(
            second,
            Resolution::Unresolvable,
            "transient TTL must suppress immediate re-hammering of a sick upstream",
        );
    }

    #[tokio::test]
    async fn slingshot_transient_recorded_separately_from_unresolvable() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/xrpc/com.atproto.repo.getRecord"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;

        let client =
            SlingshotClient::with_default_http(url::Url::parse(&server.uri()).unwrap()).unwrap();
        let resolver =
            RepoIdResolver::with_slingshot(client, test_clock(), RuntimeHasher::default());

        let owner = did("did:plc:nel");
        let key = rkey("abcabcabcabcz");
        resolver.resolve(&owner, &key).await;
        resolver.resolve(&owner, &key).await;

        let snap = resolver.stats();
        assert_eq!(
            snap.misses_transient, 1,
            "second resolve must hit the short-TTL cache instead of re-firing the transient miss",
        );
        assert_eq!(snap.hits, 1, "second call hits cached transient entry");
        assert_eq!(
            snap.misses_unresolvable, 0,
            "canonical unresolvable counter is reserved for cached terminal answers",
        );
        assert!(
            snap.miss_latency_micros_sum > 0,
            "transient misses still have latency contributions",
        );
        assert_eq!(snap.miss_count(), 1);
    }

    #[tokio::test]
    async fn slingshot_in_flight_requests_coalesce() {
        let server = wiremock::MockServer::start().await;
        let body = serde_json::json!({
            "uri": "at://did:plc:nel/sh.tangled.repo/abcabcabcabcz",
            "cid": "bafyreieqygohnz2zqyvtvktbjpvhutphobcmbsnt4q5lc36ri7vpcmoz4i",
            "value": {"$type": "sh.tangled.repo", "knot": "oyster.cafe", "createdAt": "2026-05-01T00:00:00Z", "repoDid": "did:plc:limpet"}
        });
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/xrpc/com.atproto.repo.getRecord"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(body)
                    .set_delay(Duration::from_millis(200)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client =
            SlingshotClient::with_default_http(url::Url::parse(&server.uri()).unwrap()).unwrap();
        let resolver = Arc::new(RepoIdResolver::with_slingshot(
            client,
            test_clock(),
            RuntimeHasher::default(),
        ));

        let owner = did("did:plc:nel");
        let key = rkey("abcabcabcabcz");
        let r0 = resolver.clone();
        let r1 = resolver.clone();
        let r2 = resolver.clone();
        let o0 = owner.clone();
        let o1 = owner.clone();
        let o2 = owner.clone();
        let k0 = key.clone();
        let k1 = key.clone();
        let k2 = key.clone();
        let (a, b, c) = tokio::join!(
            tokio::spawn(async move { r0.resolve(&o0, &k0).await }),
            tokio::spawn(async move { r1.resolve(&o1, &k1).await }),
            tokio::spawn(async move { r2.resolve(&o2, &k2).await }),
        );
        let expected = Resolution::Mapped(did("did:plc:limpet"));
        assert_eq!(a.unwrap(), expected);
        assert_eq!(b.unwrap(), expected);
        assert_eq!(c.unwrap(), expected);

        let snap = resolver.stats();
        assert_eq!(
            snap.misses_mapped, 1,
            "only the winning task pays the slingshot RTT",
        );
    }

    #[tokio::test]
    async fn stats_count_hits_misses_and_latency() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/xrpc/com.atproto.repo.getRecord"))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let client =
            SlingshotClient::with_default_http(url::Url::parse(&server.uri()).unwrap()).unwrap();
        let resolver =
            RepoIdResolver::with_slingshot(client, test_clock(), RuntimeHasher::default());

        let owner = did("did:plc:nel");
        let key = rkey("abcabcabcabcz");
        resolver.resolve(&owner, &key).await;
        resolver.resolve(&owner, &key).await;

        let snap = resolver.stats();
        assert_eq!(
            snap.misses_unresolvable, 1,
            "first call is the slingshot miss"
        );
        assert_eq!(snap.hits, 1, "second call hits the unresolvable cache");
        assert_eq!(snap.miss_count(), 1);
        assert_eq!(snap.total(), 2);
        assert!(
            snap.miss_latency_micros_sum > 0,
            "latency recorded for slingshot miss"
        );
        assert!(snap.miss_latency_micros_avg().unwrap() > 0);
    }

    #[tokio::test]
    async fn stats_no_client_miss_recorded_without_latency() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        resolver
            .resolve(&did("did:plc:nel"), &rkey("abcabcabcabcz"))
            .await;
        let snap = resolver.stats();
        assert_eq!(snap.misses_no_client, 1);
        assert_eq!(snap.miss_latency_micros_sum, 0);
        assert_eq!(snap.miss_latency_micros_avg(), None);
    }

    #[tokio::test]
    async fn firehose_observe_can_demote_provisional() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let owner = did("did:plc:nel");
        let key = rkey("abcabcabcabcz");
        resolver
            .fill_provisional(
                RepoIdent::new(owner.clone(), key.clone()),
                Resolution::Mapped(did("did:plc:clam")),
            )
            .await;
        resolver.observe(owner.clone(), key.clone(), None).await;
        let got = resolver.resolve(&owner, &key).await;
        assert_eq!(
            got,
            Resolution::NoRepoDid,
            "firehose update is canonical and may legitimately remove repoDID",
        );
    }

    #[tokio::test]
    async fn forget_removes_cache_entry() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let owner = did("did:plc:nel");
        let key = rkey("abcabcabcabcz");
        resolver
            .observe(owner.clone(), key.clone(), Some(did("did:plc:clam")))
            .await;
        assert_eq!(
            resolver.cached_resolution(&owner, &key).await,
            Some(Resolution::Mapped(did("did:plc:clam"))),
        );
        resolver.forget(&owner, &key).await;
        assert_eq!(
            resolver.cached_resolution(&owner, &key).await,
            None,
            "forget must drop the entry entirely so a subsequent observe can supply fresh state",
        );
    }
}
