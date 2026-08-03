mod auth;
mod identity;
mod jwt;
mod pointer;
mod pubkeys;
mod resolve;
#[cfg(test)]
mod test_support;

pub use auth::{OauthAuthorizer, PointerAuth, PointerAuthorizer, ServiceAuth};
pub use identity::{
    IdentityError, MintNonce, PreparedRepoDid, knot_did_document, prepare_repo_did,
};
pub use jwt::{JwtError, JwtNonce, ServiceJwt};
pub use pointer::PointerReceipt;
pub use pubkeys::{KeyParseError, parse_authorized_key};
pub use resolve::{Identity, PdsEndpoint, PlcDirectory, ResolveError};

#[doc(hidden)]
pub mod fuzz {
    pub fn pubkey(data: &[u8]) {
        let _ = crate::pubkeys::offered_page(data, 100);
        let _ = crate::parse_authorized_key(&String::from_utf8_lossy(data));
    }

    pub fn did_document(data: &[u8]) {
        let did = knot_types::AccountDid::new("did:plc:nel").expect("constant test did is valid");
        let _ = crate::resolve::identity_from_document(&did, data);
        let repo_did = knot_types::RepoDid::new("did:plc:nel").expect("constant test did is valid");
        let _ = crate::resolve::document_publishes_key(
            &repo_did,
            data,
            &knot_runtime::PublicKeyBytes::from_bytes(data.to_vec()),
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordPresence {
    Present,
    Absent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayGuard {
    SingleUse,
    ReusableUntilExpiry,
}

use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, TryStreamExt};
use http::StatusCode;
use knot_cache::{
    Admitted, AsyncCache, EntryCount, Expiring, GroupQuota, MokaFuture, Quotas, Rejected,
    TotalQuota, Weight,
};
use knot_runtime::{
    Clock, DnsTxtResolver, HttpRequest, HttpTransport, NetworkError, PublicKeyBytes, SystemDns,
    UnixMicros,
};
use knot_types::{
    AccountDid, Collection, Handle, HttpStatus, KnotId, Nsid, OfferedKey, RepoDid, RepoRkey, Rkey,
    UnixSeconds,
};
use pubkeys::Cursor;
use serde::{Deserialize, Serialize};
use url::Url;

const DEFAULT_TTL: Duration = Duration::from_secs(300);
const NEGATIVE_TTL: Duration = Duration::from_secs(30);
const STALE_TTL: Duration = Duration::from_secs(30);
const PUBKEY_PAGE_LIMIT: u16 = 100;
fn repo_collection() -> Nsid {
    Nsid::new_static("sh.tangled.repo").expect("literal nsid parses")
}
const PUBKEY_MAX_PAGES: usize = 8;
const PUBKEY_TTL: Duration = Duration::from_secs(30);
const MAX_PUBKEY_CACHE_BYTES: u64 = 1 << 20;
const PUBKEY_CACHE_ENTRY_OVERHEAD: u64 = 128;
const PUBKEY_CACHE_KEY_OVERHEAD: u64 = 192;
const MAX_IDENTITY_CACHE: usize = 4096;
const MAX_SEEN_JTI: usize = 8192;

const MAX_JTI_PER_ISSUER: usize = MAX_SEEN_JTI / 16;

#[derive(Debug, thiserror::Error)]
pub enum AtprotoError {
    #[error(transparent)]
    Resolve(#[from] ResolveError),
    #[error(transparent)]
    Jwt(#[from] JwtError),
    #[error("network failure: {0}")]
    Network(#[from] NetworkError),
    #[error("listRecords for {did} returned HTTP {status}")]
    ListRecords { did: AccountDid, status: HttpStatus },
    #[error("listRecords response wasn't valid JSON: {0}")]
    MalformedRecords(String),
    #[error("PDS endpoint {pds:?} isn't a usable base URL")]
    BadPdsEndpoint { pds: String },
    #[error("service token {jti:?} has already been presented")]
    Replay { jti: JwtNonce },
    #[error("replay-protection store is full and cannot accept another nonce")]
    ReplayStoreSaturated,
    #[error("issuer {issuer} has too many live replay nonces")]
    ReplayShareExhausted { issuer: AccountDid },
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error("plc submission for {did} returned HTTP {status}")]
    PlcSubmit { did: RepoDid, status: HttpStatus },
    #[error("putRecord for {subject} returned HTTP {status}")]
    PutRecord {
        subject: AccountDid,
        status: HttpStatus,
    },
    #[error("getRecord for {owner} returned HTTP {status}")]
    GetRecord {
        owner: AccountDid,
        status: HttpStatus,
    },
    #[error("pointer record couldn't be encoded: {0}")]
    PointerEncode(String),
    #[error("putRecord response isn't a valid receipt: {0}")]
    MalformedReceipt(String),
}

impl AtprotoError {
    pub fn is_transient(&self) -> bool {
        match self {
            AtprotoError::Network(_)
            | AtprotoError::ReplayStoreSaturated
            | AtprotoError::ReplayShareExhausted { .. } => true,
            AtprotoError::Resolve(error) => error.is_transient(),
            AtprotoError::ListRecords { status, .. }
            | AtprotoError::PlcSubmit { status, .. }
            | AtprotoError::PutRecord { status, .. }
            | AtprotoError::GetRecord { status, .. } => status.is_transient(),
            _ => false,
        }
    }

    pub fn is_gone(&self) -> bool {
        match self {
            AtprotoError::Resolve(error) => error.is_gone(),
            AtprotoError::Jwt(_)
            | AtprotoError::Network(_)
            | AtprotoError::ListRecords { .. }
            | AtprotoError::MalformedRecords(_)
            | AtprotoError::BadPdsEndpoint { .. }
            | AtprotoError::Replay { .. }
            | AtprotoError::ReplayStoreSaturated
            | AtprotoError::ReplayShareExhausted { .. }
            | AtprotoError::Identity(_)
            | AtprotoError::PlcSubmit { .. }
            | AtprotoError::PutRecord { .. }
            | AtprotoError::GetRecord { .. }
            | AtprotoError::PointerEncode(_)
            | AtprotoError::MalformedReceipt(_) => false,
        }
    }
}

#[derive(Clone)]
enum Resolution {
    Found(Identity),
    Failed(ResolveError),
    Transient(ResolveError),
}

#[derive(Clone)]
enum HandleResolution {
    Bound(AccountDid),
    Unbound(ResolveError),
    Transient(ResolveError),
}

#[derive(Clone)]
struct Cached<R> {
    resolution: R,
    expires_at: UnixMicros,
}

#[derive(Clone)]
pub enum ClaimedKeys {
    Published(Vec<OfferedKey>),
    Unread(Arc<AtprotoError>),
}

fn claimed_weight(cached: &Cached<ClaimedKeys>) -> Weight {
    Weight::new(match &cached.resolution {
        ClaimedKeys::Published(keys) => keys
            .iter()
            .map(|key| key.as_bytes().len() as u64 + PUBKEY_CACHE_KEY_OVERHEAD)
            .sum::<u64>()
            .saturating_add(PUBKEY_CACHE_ENTRY_OVERHEAD),
        ClaimedKeys::Unread(_) => PUBKEY_CACHE_ENTRY_OVERHEAD,
    })
}

pub struct Atproto<H, C> {
    http: H,
    clock: C,
    knot_did: KnotId,
    plc_directory: PlcDirectory,
    dns: Arc<dyn DnsTxtResolver>,
    identities: MokaFuture<AccountDid, Cached<Resolution>>,
    handles: MokaFuture<Handle, Cached<HandleResolution>>,
    claimed: MokaFuture<AccountDid, Cached<ClaimedKeys>>,
    seen_jti: Expiring<(AccountDid, JwtNonce), AccountDid, ()>,
}

impl<H: HttpTransport, C: Clock> Atproto<H, C> {
    pub fn new(http: H, clock: C, knot_did: KnotId, plc_directory: PlcDirectory) -> Self {
        Self {
            http,
            clock,
            knot_did,
            plc_directory,
            dns: Arc::new(SystemDns::new()),
            identities: MokaFuture::by_count(EntryCount::new(MAX_IDENTITY_CACHE as u64)),
            handles: MokaFuture::by_count(EntryCount::new(MAX_IDENTITY_CACHE as u64)),
            claimed: MokaFuture::by_weight(Weight::new(MAX_PUBKEY_CACHE_BYTES), claimed_weight),
            seen_jti: Expiring::new(Quotas {
                per_group: GroupQuota::new(MAX_JTI_PER_ISSUER),
                total: TotalQuota::new(MAX_SEEN_JTI),
            }),
        }
    }

    pub fn with_dns(mut self, dns: Arc<dyn DnsTxtResolver>) -> Self {
        self.dns = dns;
        self
    }

    pub fn now(&self) -> UnixMicros {
        self.clock.now_unix_micros()
    }

    pub async fn resolve_identity(&self, did: &AccountDid) -> Result<Identity, AtprotoError> {
        self.resolve_identity_inner(did).await.map_err(Into::into)
    }

    async fn resolve_identity_inner(&self, did: &AccountDid) -> Result<Identity, ResolveError> {
        let now = self.clock.now_unix_micros();
        let filled = self
            .identities
            .get_or_fill_if(
                did.clone(),
                |cached: &Cached<Resolution>| cached.expires_at <= now,
                self.fill_identity(did, now),
            )
            .await;
        let fresh = filled.fresh;
        match filled.value.resolution {
            Resolution::Found(identity) => Ok(identity),
            Resolution::Transient(error) => Err(error),
            Resolution::Failed(error) if fresh || error.is_gone() => Err(error),
            Resolution::Failed(_) => Err(ResolveError::RecentlyFailed { did: did.clone() }),
        }
    }

    pub async fn resolve_handle_to_did(&self, handle: &Handle) -> Result<AccountDid, AtprotoError> {
        let now = self.clock.now_unix_micros();
        let filled = self
            .handles
            .get_or_fill_if(
                handle.clone(),
                |cached: &Cached<HandleResolution>| cached.expires_at <= now,
                self.fill_handle(handle, now),
            )
            .await;
        let fresh = filled.fresh;
        match filled.value.resolution {
            HandleResolution::Bound(did) => Ok(did),
            HandleResolution::Transient(error) => Err(error.into()),
            HandleResolution::Unbound(error) if fresh => Err(error.into()),
            HandleResolution::Unbound(_) => Err(ResolveError::HandleRecentlyFailed {
                handle: handle.clone(),
            }
            .into()),
        }
    }

    async fn stale_handle_did(&self, handle: &Handle) -> Option<AccountDid> {
        // `fill_handle` calls this from inside its own
        // `or_insert_with_if` init for this key.
        // Moka will keep the prior entry readable until init returns,
        // so this `get` will yield the last resolved DID to re-serve,
        // when there's a temporary outage.
        match self.handles.get(handle).await?.resolution {
            HandleResolution::Bound(did) => Some(did),
            _ => None,
        }
    }

    async fn fill_handle(&self, handle: &Handle, now: UnixMicros) -> Cached<HandleResolution> {
        match self.verify_handle(handle).await {
            Ok(did) => Cached {
                resolution: HandleResolution::Bound(did),
                expires_at: expires(now, DEFAULT_TTL),
            },
            Err(error) if error.is_transient() => match self.stale_handle_did(handle).await {
                Some(did) => Cached {
                    resolution: HandleResolution::Bound(did),
                    expires_at: expires(now, STALE_TTL),
                },
                None => Cached {
                    resolution: HandleResolution::Transient(error),
                    expires_at: now,
                },
            },
            Err(error) => Cached {
                resolution: HandleResolution::Unbound(error),
                expires_at: expires(now, NEGATIVE_TTL),
            },
        }
    }

    async fn verify_handle(&self, handle: &Handle) -> Result<AccountDid, ResolveError> {
        let candidate = match self.dns_txt_did(handle).await {
            Ok(Some(did)) => did,
            Ok(None) => self.wellknown_did(handle).await?,
            Err(dns_error) if dns_error.is_transient() => match self.wellknown_did(handle).await {
                Ok(did) => did,
                Err(_) => return Err(dns_error),
            },
            Err(dns_error) => return Err(dns_error),
        };
        let identity = self.resolve_identity_inner(&candidate).await?;
        if identity.claims_handle(handle) {
            Ok(candidate)
        } else {
            Err(ResolveError::HandleMismatch {
                handle: handle.clone(),
                resolved: candidate,
                claimed: identity.primary_handle().cloned(),
            })
        }
    }

    async fn dns_txt_did(&self, handle: &Handle) -> Result<Option<AccountDid>, ResolveError> {
        let records = self
            .dns
            .lookup_txt(format!("_atproto.{}", handle.as_str()))
            .await?;
        let dids = records
            .iter()
            .filter_map(|record| record.trim().strip_prefix("did=").map(str::trim))
            .map(|value| {
                AccountDid::new(value).map_err(|_| ResolveError::HandleForwardMalformed {
                    handle: handle.clone(),
                    value: value.to_string(),
                })
            })
            .collect::<Result<Vec<AccountDid>, ResolveError>>()?;
        let distinct = dids
            .iter()
            .map(AccountDid::as_str)
            .collect::<std::collections::BTreeSet<&str>>()
            .len();
        match distinct {
            0 => Ok(None),
            1 => Ok(dids.into_iter().next()),
            _ => Err(ResolveError::HandleAmbiguous {
                handle: handle.clone(),
            }),
        }
    }

    async fn wellknown_did(&self, handle: &Handle) -> Result<AccountDid, ResolveError> {
        let url = Url::parse(&format!(
            "https://{}/.well-known/atproto-did",
            handle.as_str()
        ))
        .map_err(|_| ResolveError::HandleUnresolvable {
            handle: handle.clone(),
        })?;
        resolve::guard_fetch_url(&url)?;
        let response = self
            .http
            .execute(HttpRequest::get(url))
            .await
            .map_err(ResolveError::from)?;
        if !response.status.is_success() {
            let status = HttpStatus::from(response.status);
            if status.is_transient() {
                return Err(ResolveError::Status { status });
            }
            return Err(ResolveError::HandleUnresolvable {
                handle: handle.clone(),
            });
        }
        let value = std::str::from_utf8(&response.body)
            .map_err(|_| ResolveError::HandleUnresolvable {
                handle: handle.clone(),
            })?
            .trim();
        AccountDid::new(value).map_err(|_| ResolveError::HandleForwardMalformed {
            handle: handle.clone(),
            value: value.to_string(),
        })
    }

    async fn fill_identity(&self, did: &AccountDid, now: UnixMicros) -> Cached<Resolution> {
        match self.fetch_identity(did).await {
            Ok(identity) => Cached {
                resolution: Resolution::Found(identity),
                expires_at: expires(now, DEFAULT_TTL),
            },
            Err(error) if warrants_negative_cache(&error) => Cached {
                resolution: Resolution::Failed(error),
                expires_at: expires(now, NEGATIVE_TTL),
            },
            Err(error) => Cached {
                resolution: Resolution::Transient(error),
                expires_at: now,
            },
        }
    }

    async fn fetch_identity(&self, did: &AccountDid) -> Result<Identity, ResolveError> {
        let url = resolve::document_url(did, &self.plc_directory)?;
        resolve::guard_fetch_url(&url)?;
        let response = self
            .http
            .execute(HttpRequest::get(url))
            .await
            .map_err(ResolveError::from)?;
        if !response.status.is_success() {
            let status = HttpStatus::from(response.status);
            return match status.get() {
                404 | 410 => Err(ResolveError::Gone {
                    did: did.clone(),
                    status,
                }),
                _ => Err(ResolveError::Status { status }),
            };
        }
        resolve::identity_from_document(did, &response.body)
    }

    pub fn document_url(&self, did: &AccountDid) -> Result<Url, ResolveError> {
        resolve::document_url(did, &self.plc_directory)
    }

    pub async fn resolve_pubkeys(&self, did: &AccountDid) -> Result<Vec<OfferedKey>, AtprotoError> {
        let identity = self.resolve_identity(did).await?;
        self.pubkeys_at(&identity, did).await
    }

    pub async fn claimed_pubkeys(&self, did: &AccountDid) -> ClaimedKeys {
        let now = self.clock.now_unix_micros();
        let filled = self
            .claimed
            .get_or_fill_if(
                did.clone(),
                |cached: &Cached<ClaimedKeys>| cached.expires_at <= now,
                self.fill_claimed(did, now),
            )
            .await;
        filled.value.resolution
    }

    async fn fill_claimed(&self, did: &AccountDid, now: UnixMicros) -> Cached<ClaimedKeys> {
        match self.resolve_pubkeys(did).await {
            Ok(keys) => Cached {
                resolution: ClaimedKeys::Published(keys),
                expires_at: expires(now, PUBKEY_TTL),
            },
            Err(error) => Cached {
                expires_at: match error.is_transient() {
                    true => now,
                    false => expires(now, NEGATIVE_TTL),
                },
                resolution: ClaimedKeys::Unread(Arc::new(error)),
            },
        }
    }

    pub async fn pubkeys_at(
        &self,
        identity: &Identity,
        did: &AccountDid,
    ) -> Result<Vec<OfferedKey>, AtprotoError> {
        let http = &self.http;
        let pds = &identity.pds;
        let pages = stream::try_unfold(Page::First(PUBKEY_MAX_PAGES), move |state| async move {
            let (cursor, budget) = match state {
                Page::Done | Page::First(0) | Page::Next(_, 0) => {
                    return Ok::<_, AtprotoError>(None);
                }
                Page::First(budget) => (None, budget),
                Page::Next(cursor, budget) => (Some(cursor), budget),
            };
            let url = list_records_url(pds, did, cursor.as_ref())?;
            resolve::guard_fetch_url(&url)?;
            let response = http.execute(HttpRequest::get(url)).await?;
            if !response.status.is_success() {
                return Err(AtprotoError::ListRecords {
                    did: did.clone(),
                    status: HttpStatus::from(response.status),
                });
            }
            let page = pubkeys::offered_page(&response.body, PUBKEY_PAGE_LIMIT as usize)
                .map_err(|error| AtprotoError::MalformedRecords(error.to_string()))?;
            let next = match page.cursor {
                Some(cursor) => Page::Next(cursor, budget - 1),
                None => Page::Done,
            };
            Ok(Some((page.keys, next)))
        });
        pages
            .try_fold(Vec::new(), |mut acc, keys| async move {
                acc.extend(keys);
                Ok(acc)
            })
            .await
    }

    pub async fn repo_record_present(
        &self,
        owner: &AccountDid,
        rkey: &RepoRkey,
    ) -> Result<RecordPresence, AtprotoError> {
        let identity = self.resolve_identity(owner).await?;
        let url = get_record_url(&identity.pds, owner, &repo_collection(), rkey)?;
        resolve::guard_fetch_url(&url)?;
        let response = self.http.execute(HttpRequest::get(url)).await?;
        match response.status {
            status if status.is_success() => Ok(RecordPresence::Present),
            StatusCode::NOT_FOUND => Ok(RecordPresence::Absent),
            StatusCode::BAD_REQUEST if record_not_found(response.body.as_ref()) => {
                Ok(RecordPresence::Absent)
            }
            status => Err(AtprotoError::GetRecord {
                owner: owner.clone(),
                status: HttpStatus::from(status),
            }),
        }
    }

    pub async fn verify_service_jwt(
        &self,
        token: &ServiceJwt,
        method: &Nsid,
    ) -> Result<AccountDid, AtprotoError> {
        self.verify_service_jwt_guarded(token, method, ReplayGuard::SingleUse)
            .await
    }

    pub async fn verify_service_jwt_guarded(
        &self,
        token: &ServiceJwt,
        method: &Nsid,
        replay: ReplayGuard,
    ) -> Result<AccountDid, AtprotoError> {
        let parsed = jwt::parse(token)?;
        let issuer = jwt::issuer(&parsed)?;
        let now_micros = self.clock.now_unix_micros();
        let now = UnixSeconds::new((now_micros.get() / 1_000_000) as i64);

        jwt::check_claims(&parsed, &self.knot_did, method, now)?;
        let jti = jwt::nonce(&parsed)?;

        let identity = self.resolve_identity(&issuer).await?;
        jwt::verify_signature(&parsed, &identity.signing_key)?;

        match replay {
            ReplayGuard::SingleUse => {
                let exp = UnixSeconds::new(parsed.claims().exp);
                self.record_jti(&issuer, jti, exp, now_micros)?;
            }
            ReplayGuard::ReusableUntilExpiry => drop(jti),
        }
        Ok(issuer)
    }

    pub async fn submit_plc_operation(
        &self,
        prepared: &PreparedRepoDid,
    ) -> Result<(), AtprotoError> {
        let account = AccountDid::from(prepared.did.clone());
        let url = resolve::document_url(&account, &self.plc_directory)?;
        resolve::guard_fetch_url(&url)?;
        let request = json_post(
            url,
            bytes::Bytes::copy_from_slice(prepared.operation_json()),
        );
        let response = self.http.execute(request).await?;
        if response.status.is_success() {
            Ok(())
        } else {
            Err(AtprotoError::PlcSubmit {
                did: prepared.did.clone(),
                status: HttpStatus::from(response.status),
            })
        }
    }

    pub async fn publish_pointer<R: Collection + Serialize>(
        &self,
        authorizer: &dyn PointerAuthorizer,
        subject: &AccountDid,
        rkey: &Rkey,
        record: &R,
    ) -> Result<PointerReceipt, AtprotoError> {
        let identity = self.resolve_identity(subject).await?;
        let method = pointer::put_record_method();
        let url = xrpc_url(&identity.pds, &method)?;
        resolve::guard_fetch_url(&url)?;
        let audience = pointer::pds_service_did(&identity.pds)?;
        let now = UnixSeconds::new((self.clock.now_unix_micros().get() / 1_000_000) as i64);
        let body = pointer::put_record_body(subject, rkey, record)?;
        let mut request = json_post(url, bytes::Bytes::from(body));
        authorizer.authorize(
            &mut request,
            &PointerAuth {
                issuer: &self.knot_did,
                audience: &audience,
                lxm: &method,
                now_unix: now,
            },
        )?;
        let response = self.http.execute(request).await?;
        if !response.status.is_success() {
            return Err(AtprotoError::PutRecord {
                subject: subject.clone(),
                status: HttpStatus::from(response.status),
            });
        }
        pointer::receipt_from_response(&response.body)
    }

    pub async fn verify_did_web_publishes_key(
        &self,
        did: &RepoDid,
        expected: &PublicKeyBytes,
    ) -> Result<(), AtprotoError> {
        let url = resolve::web_document_url_for(did)?;
        resolve::guard_fetch_url(&url)?;
        let response = self
            .http
            .execute(HttpRequest::get(url))
            .await
            .map_err(ResolveError::from)?;
        if !response.status.is_success() {
            return Err(ResolveError::Status {
                status: HttpStatus::from(response.status),
            }
            .into());
        }
        resolve::document_publishes_key(did, &response.body, expected).map_err(Into::into)
    }

    fn record_jti(
        &self,
        issuer: &AccountDid,
        jti: JwtNonce,
        exp: UnixSeconds,
        now: UnixMicros,
    ) -> Result<(), AtprotoError> {
        let horizon = exp.saturating_add_secs(jwt::CLOCK_SKEW_SECS).get().max(0) as u64;
        let expires_at = UnixMicros::new(horizon.saturating_mul(1_000_000));
        match self.seen_jti.admit(
            (issuer.clone(), jti.clone()),
            issuer.clone(),
            (),
            expires_at,
            now,
        ) {
            Ok(Admitted::Inserted) => Ok(()),
            Ok(Admitted::Occupied(())) => Err(AtprotoError::Replay { jti }),
            Err(Rejected::Total) => Err(AtprotoError::ReplayStoreSaturated),
            Err(Rejected::Group) => Err(AtprotoError::ReplayShareExhausted {
                issuer: issuer.clone(),
            }),
        }
    }
}

fn expires(now: UnixMicros, ttl: Duration) -> UnixMicros {
    let micros = u64::try_from(ttl.as_micros()).unwrap_or(u64::MAX);
    UnixMicros::new(now.get().saturating_add(micros))
}

fn warrants_negative_cache(error: &ResolveError) -> bool {
    match error {
        ResolveError::Status { status } => {
            (400..500).contains(&status.get()) && status.get() != 429
        }
        ResolveError::Gone { .. }
        | ResolveError::Malformed(_)
        | ResolveError::IdMismatch { .. }
        | ResolveError::BadSigningKey(_)
        | ResolveError::BadPds { .. } => true,
        _ => false,
    }
}

enum Page {
    First(usize),
    Next(Cursor, usize),
    Done,
}

fn json_post(url: Url, body: bytes::Bytes) -> HttpRequest {
    let mut request = HttpRequest::post(url, body);
    request.headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    request
}

fn xrpc_url(pds: &PdsEndpoint, method: &Nsid) -> Result<Url, AtprotoError> {
    let mut url = pds.url().clone();
    url.path_segments_mut()
        .map_err(|_| AtprotoError::BadPdsEndpoint {
            pds: pds.url().as_str().to_string(),
        })?
        .pop_if_empty()
        .extend(["xrpc", method.as_str()]);
    Ok(url)
}

fn list_records_url(
    pds: &PdsEndpoint,
    did: &AccountDid,
    cursor: Option<&Cursor>,
) -> Result<Url, AtprotoError> {
    let method: Nsid =
        Nsid::new_static("com.atproto.repo.listRecords").expect("literal nsid parses");
    let collection: Nsid = Nsid::new_static("sh.tangled.publicKey").expect("literal nsid parses");
    let mut url = xrpc_url(pds, &method)?;
    url.query_pairs_mut()
        .append_pair("repo", did.as_str())
        .append_pair("collection", collection.as_str())
        .append_pair("limit", &PUBKEY_PAGE_LIMIT.to_string());
    if let Some(cursor) = cursor {
        url.query_pairs_mut().append_pair("cursor", cursor.as_str());
    }
    Ok(url)
}

#[derive(Deserialize)]
struct XrpcErrorBody {
    error: String,
}

fn record_not_found(body: &[u8]) -> bool {
    serde_json::from_slice::<XrpcErrorBody>(body)
        .is_ok_and(|parsed| parsed.error == "RecordNotFound")
}

fn get_record_url(
    pds: &PdsEndpoint,
    owner: &AccountDid,
    collection: &Nsid,
    rkey: &RepoRkey,
) -> Result<Url, AtprotoError> {
    let method: Nsid = Nsid::new_static("com.atproto.repo.getRecord").expect("literal nsid parses");
    let mut url = xrpc_url(pds, &method)?;
    url.query_pairs_mut()
        .append_pair("repo", owner.as_str())
        .append_pair("collection", collection.as_str())
        .append_pair("rkey", rkey.as_str());
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use bytes::Bytes;
    use futures::StreamExt;
    use http::StatusCode;
    use knot_runtime::{DnsTxtResolver, FakeDns, FakeHttp, ManualClock, NetworkError};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    const POINTER_RKEY: &str = "3jzfcijpj2z2a";
    const POINTER_CID: &str = "bafyreidfayvfuwqa7qlnopdjiqrxzs6blmoeu4rujcjtnci5beludirz2a";

    fn squid_doc(signing: &k256::ecdsa::SigningKey) -> Bytes {
        did_doc(DocSpec {
            id: SQUID,
            signing,
            handle: "nel.pet",
            pds: "https://pds.oyster.cafe",
            method: MethodKind::Multikey,
        })
    }

    fn resolver<T: HttpTransport>(dns: impl DnsTxtResolver, http: T) -> Atproto<T, ManualClock> {
        Atproto::new(http, clock(), knot_did(KNOT), plc()).with_dns(Arc::new(dns))
    }

    #[tokio::test]
    async fn an_identity_is_resolved_from_a_did_document() {
        let signing = signer(9);
        let http = FakeHttp::new(move |request| {
            assert_eq!(request.url.as_str(), "https://plc.directory/did:plc:squid");
            Ok(ok(squid_doc(&signing)))
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let identity = atproto.resolve_identity(&did(SQUID)).await.unwrap();
        assert_eq!(identity.pds.url().as_str(), "https://pds.oyster.cafe/");
        assert_eq!(identity.primary_handle().unwrap().as_str(), "nel.pet");
    }

    #[tokio::test]
    async fn a_second_resolution_is_served_from_cache() {
        let signing = signer(9);
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let http = FakeHttp::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(ok(squid_doc(&signing)))
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        atproto.resolve_identity(&did(SQUID)).await.unwrap();
        atproto.resolve_identity(&did(SQUID)).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_expired_cache_entry_is_refetched() {
        let signing = signer(9);
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let http = FakeHttp::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(ok(squid_doc(&signing)))
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        atproto.resolve_identity(&did(SQUID)).await.unwrap();
        atproto
            .clock
            .advance(DEFAULT_TTL + Duration::from_micros(1));
        atproto.resolve_identity(&did(SQUID)).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_handle_resolves_bidirectionally_via_dns_and_is_cached() {
        let signing = signer(9);
        let dns_hits = Arc::new(AtomicUsize::new(0));
        let counter = dns_hits.clone();
        let dns = FakeDns::new(move |name: &str| {
            assert_eq!(name, "_atproto.nel.pet");
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(vec!["did=did:plc:squid".to_string()])
        });
        let http = FakeHttp::new(move |request| {
            assert_eq!(request.url.as_str(), "https://plc.directory/did:plc:squid");
            let mut doc: serde_json::Value = serde_json::from_slice(&squid_doc(&signing)).unwrap();
            doc["alsoKnownAs"] = serde_json::json!(["at://olaren.dev", "at://nel.pet"]);
            Ok(ok(Bytes::from(serde_json::to_vec(&doc).unwrap())))
        });
        let atproto = resolver(dns, http);
        let owner = handle("nel.pet");
        assert_eq!(
            atproto
                .resolve_handle_to_did(&owner)
                .await
                .unwrap()
                .as_str(),
            "did:plc:squid",
            "a handle listed anywhere in alsoKnownAs must resolve, even when it isn't first"
        );
        assert_eq!(
            atproto
                .resolve_handle_to_did(&owner)
                .await
                .unwrap()
                .as_str(),
            "did:plc:squid"
        );
        assert_eq!(
            dns_hits.load(Ordering::SeqCst),
            1,
            "a resolved handle is served from cache"
        );
    }

    #[tokio::test]
    async fn a_handle_falls_back_to_well_known_when_dns_is_empty_or_transient() {
        let well_known = || {
            let signing = signer(9);
            FakeHttp::new(move |request| match request.url.as_str() {
                "https://nel.pet/.well-known/atproto-did" => {
                    Ok(ok(Bytes::from_static(b"did:plc:squid\n")))
                }
                "https://plc.directory/did:plc:squid" => Ok(ok(squid_doc(&signing))),
                other => panic!("unexpected url {other}"),
            })
        };
        let empty = resolver(FakeDns::new(|_| Ok(Vec::new())), well_known());
        let transient = resolver(
            FakeDns::new(|_| Err(NetworkError::Request("dns unreachable".to_string()))),
            well_known(),
        );
        for atproto in [empty, transient] {
            assert_eq!(
                atproto
                    .resolve_handle_to_did(&handle("nel.pet"))
                    .await
                    .unwrap()
                    .as_str(),
                "did:plc:squid"
            );
        }
    }

    #[tokio::test]
    async fn a_handle_is_rejected_when_ambiguous_or_disowned() {
        let ambiguous = resolver(
            FakeDns::new(|_| {
                Ok(vec![
                    "did=did:plc:squid".to_string(),
                    "did=did:plc:limpet".to_string(),
                ])
            }),
            FakeHttp::new(|_| panic!("resolution must stop before any fetch")),
        );
        assert!(matches!(
            ambiguous
                .resolve_handle_to_did(&handle("nel.pet"))
                .await
                .unwrap_err(),
            AtprotoError::Resolve(ResolveError::HandleAmbiguous { .. })
        ));

        let signing = signer(9);
        let disowned = resolver(
            FakeDns::new(|_| Ok(vec!["did=did:plc:squid".to_string()])),
            FakeHttp::new(move |_| Ok(ok(squid_doc(&signing)))),
        );
        assert!(matches!(
            disowned
                .resolve_handle_to_did(&handle("olaren.dev"))
                .await
                .unwrap_err(),
            AtprotoError::Resolve(ResolveError::HandleMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn handle_failures_cache_by_class() {
        let owner = handle("nel.pet");

        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let unresolvable = resolver(
            FakeDns::new(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            }),
            FakeHttp::new(|_| Ok(status(StatusCode::NOT_FOUND, Bytes::new()))),
        );
        assert!(matches!(
            unresolvable
                .resolve_handle_to_did(&owner)
                .await
                .unwrap_err(),
            AtprotoError::Resolve(ResolveError::HandleUnresolvable { .. })
        ));
        assert!(matches!(
            unresolvable
                .resolve_handle_to_did(&owner)
                .await
                .unwrap_err(),
            AtprotoError::Resolve(ResolveError::HandleRecentlyFailed { .. })
        ));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "an unresolvable handle is resolved once then served from the negative cache"
        );

        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let transient = resolver(
            FakeDns::new(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            }),
            FakeHttp::new(|_| Ok(status(StatusCode::SERVICE_UNAVAILABLE, Bytes::new()))),
        );
        transient.resolve_handle_to_did(&owner).await.unwrap_err();
        transient.resolve_handle_to_did(&owner).await.unwrap_err();
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "a transient well-known status mustn't be negatively cached"
        );
    }

    #[tokio::test]
    async fn a_transient_outage_serves_the_last_good_did() {
        let signing = signer(9);
        let outage = Arc::new(AtomicUsize::new(0));
        let switch = outage.clone();
        let dns = FakeDns::new(move |_| match switch.load(Ordering::SeqCst) {
            0 => Ok(vec!["did=did:plc:squid".to_string()]),
            _ => Err(NetworkError::Request("dns unreachable".to_string())),
        });
        let atproto = resolver(dns, FakeHttp::new(move |_| Ok(ok(squid_doc(&signing)))));
        let owner = handle("nel.pet");
        assert_eq!(
            atproto
                .resolve_handle_to_did(&owner)
                .await
                .unwrap()
                .as_str(),
            "did:plc:squid"
        );

        atproto.clock.advance(DEFAULT_TTL + Duration::from_secs(1));
        outage.store(1, Ordering::SeqCst);
        assert_eq!(
            atproto
                .resolve_handle_to_did(&owner)
                .await
                .unwrap()
                .as_str(),
            "did:plc:squid",
            "a transient outage must serve the last resolved DID"
        );

        atproto.clock.advance(STALE_TTL + Duration::from_secs(1));
        outage.store(0, Ordering::SeqCst);
        assert_eq!(
            atproto
                .resolve_handle_to_did(&owner)
                .await
                .unwrap()
                .as_str(),
            "did:plc:squid"
        );
    }

    #[tokio::test]
    async fn a_404_identity_is_negatively_cached() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let http = FakeHttp::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(status(StatusCode::NOT_FOUND, Bytes::new()))
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let first = atproto.resolve_identity(&did(SQUID)).await.unwrap_err();
        assert!(
            matches!(first, AtprotoError::Resolve(ResolveError::Gone { status, .. }) if status.get() == 404),
            "got {first:?}"
        );
        let second = atproto.resolve_identity(&did(SQUID)).await.unwrap_err();
        assert!(
            matches!(second, AtprotoError::Resolve(ResolveError::Gone { .. })),
            "a caller that acts on a missing account must see the same answer from the cache as \
             from the fetch, or it acts on the first read only: got {second:?}"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "404 is served from the negative cache"
        );
    }

    #[tokio::test]
    async fn a_transient_5xx_identity_is_not_cached() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let http = FakeHttp::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(status(StatusCode::SERVICE_UNAVAILABLE, Bytes::new()))
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let _ = atproto.resolve_identity(&did(SQUID)).await.unwrap_err();
        let _ = atproto.resolve_identity(&did(SQUID)).await.unwrap_err();
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "a transient 503 is re-fetched instead of negatively cached"
        );
    }

    #[tokio::test]
    async fn a_document_describing_another_did_is_rejected() {
        let doc_key = signer(7);
        let http = FakeHttp::new(move |_| {
            Ok(ok(did_doc(DocSpec {
                id: LIMPET,
                signing: &doc_key,
                handle: "nel.pet",
                pds: "https://pds.oyster.cafe",
                method: MethodKind::Multikey,
            })))
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let error = atproto.resolve_identity(&did(SQUID)).await.unwrap_err();
        assert!(
            matches!(
                error,
                AtprotoError::Resolve(ResolveError::IdMismatch { .. })
            ),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn a_claimed_handle_is_returned_unverified() {
        let signing = signer(9);
        let http = FakeHttp::new(move |_| {
            Ok(ok(did_doc(DocSpec {
                id: SQUID,
                signing: &signing,
                handle: "olaren.dev",
                pds: "https://pds.oyster.cafe",
                method: MethodKind::Multikey,
            })))
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let identity = atproto.resolve_identity(&did(SQUID)).await.unwrap();
        assert_eq!(
            identity.primary_handle().unwrap().as_str(),
            "olaren.dev",
            "alsoKnownAs handle is taken at face value with no bidirectional verification"
        );
    }

    #[tokio::test]
    async fn an_internal_ip_with_a_port_is_refused() {
        let (sink, urls) = recorder();
        let http = FakeHttp::new(move |request| {
            sink.lock().unwrap().push(request.url.clone());
            Ok(ok(Bytes::new()))
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let error = atproto
            .resolve_identity(&did("did:web:169.254.169.254%3A6379"))
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                AtprotoError::Resolve(ResolveError::BlockedHost { .. })
            ),
            "got {error:?}"
        );
        assert!(urls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn repo_record_presence_maps_pds_status_to_a_verdict() {
        let signing = signer(9);
        let http = FakeHttp::new(move |request| {
            if request.url.path().ends_with("did.json")
                || request.url.host_str() == Some("plc.directory")
            {
                return Ok(ok(squid_doc(&signing)));
            }
            assert!(request.url.path().ends_with("com.atproto.repo.getRecord"));
            assert!(request.url.query().unwrap().contains("sh.tangled.repo"));
            let rkey = request
                .url
                .query_pairs()
                .find(|(key, _)| key == "rkey")
                .map(|(_, value)| value.into_owned())
                .unwrap_or_default();
            let (st, body) = match rkey.as_str() {
                "present" => (StatusCode::OK, Bytes::new()),
                "missing" => (
                    StatusCode::BAD_REQUEST,
                    Bytes::from_static(b"{\"error\":\"RecordNotFound\"}"),
                ),
                _ => (StatusCode::INTERNAL_SERVER_ERROR, Bytes::new()),
            };
            Ok(status(st, body))
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let owner = did(SQUID);
        assert_eq!(
            atproto
                .repo_record_present(&owner, &RepoRkey::new("present").unwrap())
                .await
                .unwrap(),
            RecordPresence::Present
        );
        assert_eq!(
            atproto
                .repo_record_present(&owner, &RepoRkey::new("missing").unwrap())
                .await
                .unwrap(),
            RecordPresence::Absent
        );
        assert!(
            atproto
                .repo_record_present(&owner, &RepoRkey::new("boom").unwrap())
                .await
                .is_err(),
            "5xx from the PDS surfaces as an error so the caller can fall back to best-effort"
        );
    }

    #[tokio::test]
    async fn pubkey_resolution_follows_the_cursor() {
        let doc_key = signer(9);
        let list_hits = Arc::new(AtomicUsize::new(0));
        let counter = list_hits.clone();
        let page_one = list_body(
            &[ssh_line("ssh-ed25519", &[1u8; 32], "one")],
            Some("page-2"),
        );
        let page_two = list_body(&[ssh_line("ssh-ed25519", &[2u8; 32], "two")], None);
        let http = FakeHttp::new(move |request| {
            if request.url.host_str() == Some("plc.directory") {
                return Ok(ok(squid_doc(&doc_key)));
            }
            counter.fetch_add(1, Ordering::SeqCst);
            let on_second = request.url.query().unwrap().contains("cursor=page-2");
            Ok(ok(if on_second {
                page_two.clone()
            } else {
                page_one.clone()
            }))
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let keys = atproto.resolve_pubkeys(&did(SQUID)).await.unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(list_hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_repeated_claim_is_served_from_the_cache_instead_of_the_pds() {
        let signing = signer(9);
        let line = ssh_line("ssh-ed25519", &[4u8; 32], "nel@oyster.cafe");
        let expected = parse_authorized_key(&line).unwrap();
        let listings = Arc::new(AtomicUsize::new(0));
        let counter = listings.clone();
        let body = list_body(&[line], None);
        let http = FakeHttp::new(move |request| {
            if request.url.host_str() == Some("plc.directory") {
                return Ok(ok(squid_doc(&signing)));
            }
            assert_eq!(request.url.host_str(), Some("pds.oyster.cafe"));
            assert!(request.url.path().ends_with("com.atproto.repo.listRecords"));
            assert!(
                request
                    .url
                    .query()
                    .unwrap()
                    .contains("sh.tangled.publicKey")
            );
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(ok(body.clone()))
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());

        let first = atproto.claimed_pubkeys(&did(SQUID)).await;
        assert!(
            matches!(&first, ClaimedKeys::Published(keys) if keys == std::slice::from_ref(&expected))
        );
        let second = atproto.claimed_pubkeys(&did(SQUID)).await;
        assert!(matches!(&second, ClaimedKeys::Published(keys) if keys == &[expected]));
        assert_eq!(
            listings.load(Ordering::SeqCst),
            1,
            "an ssh handshake asserting a login name reads the claimed account's records, so a \
             repeat inside the cache turn mustn't read them again, or any stranger with an ssh \
             client can make the knot fetch from that account's PDS at will"
        );

        atproto.clock.advance(PUBKEY_TTL + Duration::from_micros(1));
        let _ = atproto.claimed_pubkeys(&did(SQUID)).await;
        assert_eq!(
            listings.load(Ordering::SeqCst),
            2,
            "a key published after the last read will be read once the cache turn is over"
        );
    }

    #[tokio::test]
    async fn a_refused_claim_read_is_cached_and_an_outage_is_retried() {
        let refusal = Arc::new(Mutex::new(StatusCode::BAD_REQUEST));
        let answered = Arc::clone(&refusal);
        let signing = signer(9);
        let listings = Arc::new(AtomicUsize::new(0));
        let counter = listings.clone();
        let http = FakeHttp::new(move |request| {
            if request.url.host_str() == Some("plc.directory") {
                return Ok(ok(squid_doc(&signing)));
            }
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(status(*answered.lock().unwrap(), Bytes::new()))
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());

        assert!(matches!(
            atproto.claimed_pubkeys(&did(SQUID)).await,
            ClaimedKeys::Unread(_)
        ));
        let _ = atproto.claimed_pubkeys(&did(SQUID)).await;
        assert_eq!(
            listings.load(Ordering::SeqCst),
            1,
            "a listing the PDS refuses outright is served from the negative cache, so \
             repeating the claim mustn't read the PDS again"
        );

        atproto
            .clock
            .advance(NEGATIVE_TTL + Duration::from_micros(1));
        *refusal.lock().unwrap() = StatusCode::SERVICE_UNAVAILABLE;
        let _ = atproto.claimed_pubkeys(&did(SQUID)).await;
        let _ = atproto.claimed_pubkeys(&did(SQUID)).await;
        assert_eq!(
            listings.load(Ordering::SeqCst),
            3,
            "a transient failure is cached with no lifetime, so the account's next claim will \
             read the PDS again instead of waiting out a negative turn"
        );
    }

    #[tokio::test]
    async fn a_plain_http_pds_is_refused() {
        let doc_key = signer(9);
        let (sink, urls) = recorder();
        let http = FakeHttp::new(move |request| {
            sink.lock().unwrap().push(request.url.clone());
            if request.url.host_str() == Some("plc.directory") {
                Ok(ok(did_doc(DocSpec {
                    id: SQUID,
                    signing: &doc_key,
                    handle: "nel.pet",
                    pds: "http://127.0.0.1:6379",
                    method: MethodKind::Multikey,
                })))
            } else {
                Ok(ok(list_body(&[], None)))
            }
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let error = atproto.resolve_pubkeys(&did(SQUID)).await.unwrap_err();
        assert!(
            matches!(
                error,
                AtprotoError::Resolve(ResolveError::InsecureScheme { .. })
            ),
            "got {error:?}"
        );
        assert_eq!(urls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_single_page_is_bounded_by_the_pubkey_page_limit() {
        let signing = signer(9);
        let lines: Vec<String> = (0..3_000u32)
            .map(|seed| {
                let mut material = [0u8; 32];
                material[..4].copy_from_slice(&seed.to_be_bytes());
                ssh_line("ssh-ed25519", &material, "k")
            })
            .collect();
        let page = list_body(&lines, None);
        let http = FakeHttp::new(move |request| {
            if request.url.host_str() == Some("plc.directory") {
                Ok(ok(squid_doc(&signing)))
            } else {
                Ok(ok(page.clone()))
            }
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let keys = atproto.resolve_pubkeys(&did(SQUID)).await.unwrap();
        assert_eq!(
            keys.len(),
            PUBKEY_PAGE_LIMIT as usize,
            "single page yields at most the page limit even when the PDS floods it"
        );
    }

    #[tokio::test]
    async fn a_service_jwt_authenticates_against_the_resolved_issuer_key() {
        let signing = signer(9);
        let doc = signing.clone();
        let http = FakeHttp::new(move |_| Ok(ok(squid_doc(&doc))));
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let method = member_method();
        let claims = serde_json::json!({
            "iss": SQUID, "aud": KNOT, "exp": 1_001, "iat": 999,
            "jti": "nonce-authenticates", "lxm": METHOD,
        });
        let token = mint(&signing, &claims);
        let authed = atproto.verify_service_jwt(&token, &method).await.unwrap();
        assert_eq!(authed, did(SQUID));
    }

    #[tokio::test]
    async fn a_service_jwt_signed_by_an_impostor_is_rejected() {
        let real = signer(9);
        let impostor = signer(3);
        let http = FakeHttp::new(move |_| Ok(ok(squid_doc(&real))));
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let method = member_method();
        let claims = serde_json::json!({
            "iss": SQUID, "aud": KNOT, "exp": 1_001, "iat": 999,
            "jti": "nonce-impostor", "lxm": METHOD,
        });
        let token = mint(&impostor, &claims);
        let error = atproto
            .verify_service_jwt(&token, &method)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AtprotoError::Jwt(JwtError::InvalidSignature)
        ));
    }

    #[tokio::test]
    async fn a_replayed_token_is_rejected() {
        let key = signer(9);
        let http = FakeHttp::new(move |_| Ok(ok(squid_doc(&key))));
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let method = member_method();
        let claims = serde_json::json!({
            "iss": SQUID, "aud": KNOT, "exp": 1_001, "iat": 999,
            "jti": "single-use-nonce", "lxm": METHOD,
        });
        let token = mint(&signer(9), &claims);
        assert_eq!(
            atproto.verify_service_jwt(&token, &method).await.unwrap(),
            did(SQUID)
        );
        let replay = atproto
            .verify_service_jwt(&token, &method)
            .await
            .unwrap_err();
        assert!(
            matches!(replay, AtprotoError::Replay { .. }),
            "got {replay:?}"
        );
    }

    #[tokio::test]
    async fn two_issuers_may_share_a_nonce() {
        let squid_key = signer(1);
        let limpet_key = signer(2);
        let sq = squid_key.clone();
        let li = limpet_key.clone();
        let http = FakeHttp::new(move |request| {
            if request.url.as_str().ends_with(LIMPET) {
                Ok(ok(did_doc(DocSpec {
                    id: LIMPET,
                    signing: &li,
                    handle: "nel.pet",
                    pds: "https://pds.oyster.cafe",
                    method: MethodKind::Multikey,
                })))
            } else {
                Ok(ok(squid_doc(&sq)))
            }
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let method = member_method();
        let squid = serde_json::json!({
            "iss": SQUID, "aud": KNOT, "exp": 1_001, "iat": 999,
            "jti": "shared-nonce", "lxm": METHOD,
        });
        let limpet = serde_json::json!({
            "iss": LIMPET, "aud": KNOT, "exp": 1_001, "iat": 999,
            "jti": "shared-nonce", "lxm": METHOD,
        });
        assert_eq!(
            atproto
                .verify_service_jwt(&mint(&squid_key, &squid), &method)
                .await
                .unwrap(),
            did(SQUID)
        );
        assert_eq!(
            atproto
                .verify_service_jwt(&mint(&limpet_key, &limpet), &method)
                .await
                .unwrap(),
            did(LIMPET)
        );
    }

    #[test]
    fn knot_id_lowercases_its_host() {
        assert_eq!(knot_did("did:web:NEL.PET").as_str(), "did:web:nel.pet");
    }

    #[test]
    fn list_records_url_is_well_formed() {
        let url = list_records_url(
            &PdsEndpoint::new(Url::parse("https://pds.oyster.cafe").unwrap()).unwrap(),
            &did(SQUID),
            None,
        )
        .unwrap();
        assert_eq!(url.host_str(), Some("pds.oyster.cafe"));
        assert_eq!(url.path(), "/xrpc/com.atproto.repo.listRecords");
        let query = url.query().unwrap();
        assert!(query.contains("repo=did%3Aplc%3Asquid"));
        assert!(query.contains("collection=sh.tangled.publicKey"));
    }

    #[test]
    fn list_records_url_preserves_a_pds_base_path() {
        let url = list_records_url(
            &PdsEndpoint::new(Url::parse("https://shared.host/account-pds").unwrap()).unwrap(),
            &did(SQUID),
            Some(&Cursor::new("page2")),
        )
        .unwrap();
        assert_eq!(url.path(), "/account-pds/xrpc/com.atproto.repo.listRecords");
        assert!(url.query().unwrap().contains("cursor=page2"));
    }

    struct JwtCase {
        name: &'static str,
        header: &'static [u8],
        mutate: fn(&mut serde_json::Value),
        expect: fn(&Result<AccountDid, AtprotoError>) -> bool,
        zero_network: bool,
    }

    const JWT_HEADER: &[u8] = br#"{"alg":"ES256K","typ":"JWT"}"#;

    const JWT_CASES: &[JwtCase] = &[
        JwtCase {
            name: "internal-ip issuer is blocked before any fetch",
            header: JWT_HEADER,
            mutate: |c| c["iss"] = serde_json::json!("did:web:169.254.169.254"),
            expect: |r| {
                matches!(
                    r,
                    Err(AtprotoError::Resolve(ResolveError::BlockedHost { .. }))
                )
            },
            zero_network: true,
        },
        JwtCase {
            name: "decade-long token exceeds the lifetime limit",
            header: JWT_HEADER,
            mutate: |c| {
                c["exp"] = serde_json::json!(1000 + 315_360_000i64);
                c["iat"] = serde_json::json!(1000);
            },
            expect: |r| matches!(r, Err(AtprotoError::Jwt(JwtError::LifetimeTooLong { .. }))),
            zero_network: true,
        },
        JwtCase {
            name: "audience in a different case is accepted",
            header: JWT_HEADER,
            mutate: |c| c["aud"] = serde_json::json!("did:web:NEL.PET"),
            expect: |r| r.is_ok(),
            zero_network: false,
        },
        JwtCase {
            name: "legacy-typed issuer key verifies",
            header: JWT_HEADER,
            mutate: |c| c["iss"] = serde_json::json!(LIMPET),
            expect: |r| matches!(r, Ok(did) if did.as_str() == LIMPET),
            zero_network: false,
        },
        JwtCase {
            name: "token addressed to another knot is refused",
            header: JWT_HEADER,
            mutate: |c| c["aud"] = serde_json::json!("did:web:somewhere.else"),
            expect: |r| matches!(r, Err(AtprotoError::Jwt(JwtError::AudienceMismatch { .. }))),
            zero_network: true,
        },
        JwtCase {
            name: "stale token is expired before resolution",
            header: JWT_HEADER,
            mutate: |c| {
                c["exp"] = serde_json::json!(1);
                c["iat"] = serde_json::json!(0);
            },
            expect: |r| matches!(r, Err(AtprotoError::Jwt(JwtError::Expired { .. }))),
            zero_network: true,
        },
        JwtCase {
            name: "nonceless token is refused before resolution",
            header: JWT_HEADER,
            mutate: |c| {
                c.as_object_mut().unwrap().remove("jti");
            },
            expect: |r| matches!(r, Err(AtprotoError::Jwt(JwtError::MissingNonce))),
            zero_network: true,
        },
        JwtCase {
            name: "alg none is refused",
            header: br#"{"alg":"none","typ":"JWT"}"#,
            mutate: |_| {},
            expect: |r| matches!(r, Err(AtprotoError::Jwt(JwtError::Parse(_)))),
            zero_network: false,
        },
        JwtCase {
            name: "es256 header against a k256 doc key is refused",
            header: br#"{"alg":"ES256","typ":"JWT"}"#,
            mutate: |_| {},
            expect: |r| matches!(r, Err(AtprotoError::Jwt(JwtError::Parse(_)))),
            zero_network: false,
        },
        JwtCase {
            name: "token type that isn't JWT is refused",
            header: br#"{"alg":"ES256K","typ":"secevent+jwt"}"#,
            mutate: |_| {},
            expect: |r| matches!(r, Err(AtprotoError::Jwt(JwtError::UnexpectedType { .. }))),
            zero_network: true,
        },
        JwtCase {
            name: "method mismatch is refused before resolution",
            header: JWT_HEADER,
            mutate: |c| c["lxm"] = serde_json::json!("sh.tangled.repo.delete"),
            expect: |r| matches!(r, Err(AtprotoError::Jwt(JwtError::MethodMismatch { .. }))),
            zero_network: true,
        },
        JwtCase {
            name: "oversized nonce is refused before resolution",
            header: JWT_HEADER,
            mutate: |c| c["jti"] = serde_json::json!("n".repeat(100_000)),
            expect: |r| matches!(r, Err(AtprotoError::Jwt(JwtError::OversizedNonce { .. }))),
            zero_network: true,
        },
    ];

    #[tokio::test]
    async fn verify_service_jwt_rejects_every_malformed_or_adversarial_token() {
        let method = member_method();
        stream::iter(JWT_CASES)
            .for_each(|case| {
                let method = &method;
                async move {
                    let doc_key = signer(1);
                    let calls = Arc::new(AtomicUsize::new(0));
                    let counter = calls.clone();
                    let http = FakeHttp::new(move |request| {
                        counter.fetch_add(1, Ordering::SeqCst);
                        if request.url.as_str().ends_with(LIMPET) {
                            Ok(ok(did_doc(DocSpec {
                                id: LIMPET,
                                signing: &doc_key,
                                handle: "nel.pet",
                                pds: "https://pds.oyster.cafe",
                                method: MethodKind::LegacyK256,
                            })))
                        } else {
                            Ok(ok(squid_doc(&doc_key)))
                        }
                    });
                    let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
                    let mut claims = serde_json::json!({
                        "iss": SQUID, "aud": KNOT, "exp": 1_001, "iat": 999,
                        "jti": "nonce-3a", "lxm": METHOD,
                    });
                    (case.mutate)(&mut claims);
                    let token = mint_with_header(&signer(1), case.header, &claims);
                    let result = atproto.verify_service_jwt(&token, method).await;
                    assert!(
                        (case.expect)(&result),
                        "case {:?} got {result:?}",
                        case.name
                    );
                    if case.zero_network {
                        assert_eq!(
                            calls.load(Ordering::SeqCst),
                            0,
                            "case {:?} must be refused before any network resolution",
                            case.name
                        );
                    }
                }
            })
            .await;
    }

    #[tokio::test]
    async fn the_jti_replay_store_is_bounded_and_fails_closed_when_saturated() {
        let signing = signer(9);
        let doc = signing.clone();
        let http = FakeHttp::new(move |request| {
            let host = request.url.host_str().unwrap().to_string();
            let id = format!("did:web:{host}");
            Ok(ok(did_doc(DocSpec {
                id: &id,
                signing: &doc,
                handle: "nel.pet",
                pds: "https://pds.oyster.cafe",
                method: MethodKind::Multikey,
            })))
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let method = member_method();
        let issuers = MAX_SEEN_JTI / MAX_JTI_PER_ISSUER;
        stream::iter(
            (0..issuers).flat_map(|issuer| (0..MAX_JTI_PER_ISSUER).map(move |i| (issuer, i))),
        )
        .for_each(|(issuer, i)| {
            let atproto = &atproto;
            let method = &method;
            let signing = &signing;
            async move {
                let claims = serde_json::json!({
                    "iss": format!("did:web:i{issuer}.oyster.cafe"), "aud": KNOT,
                    "exp": 1_001, "iat": 999,
                    "jti": format!("nonce-{issuer}-{i}"), "lxm": METHOD,
                });
                atproto
                    .verify_service_jwt(&mint(signing, &claims), method)
                    .await
                    .unwrap();
            }
        })
        .await;
        assert_eq!(atproto.seen_jti.len(), MAX_SEEN_JTI);
        let overflow = serde_json::json!({
            "iss": "did:web:fresh.oyster.cafe", "aud": KNOT, "exp": 1_001, "iat": 999,
            "jti": "nonce-overflow", "lxm": METHOD,
        });
        let error = atproto
            .verify_service_jwt(&mint(&signing, &overflow), &method)
            .await
            .unwrap_err();
        assert!(
            matches!(error, AtprotoError::ReplayStoreSaturated),
            "every verify past the global limit fails closed, got {error:?}"
        );
        assert!(
            atproto.seen_jti.len() <= MAX_SEEN_JTI,
            "replay store must stay bounded, held {}",
            atproto.seen_jti.len()
        );
    }

    #[tokio::test]
    async fn one_issuer_cannot_hog_the_replay_store() {
        let signing = signer(9);
        let doc = signing.clone();
        let http = FakeHttp::new(move |request| {
            let host = request.url.host_str().unwrap().to_string();
            if host == "plc.directory" {
                Ok(ok(squid_doc(&doc)))
            } else {
                let id = format!("did:web:{host}");
                Ok(ok(did_doc(DocSpec {
                    id: &id,
                    signing: &doc,
                    handle: "nel.pet",
                    pds: "https://pds.oyster.cafe",
                    method: MethodKind::Multikey,
                })))
            }
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let method = member_method();
        stream::iter(0..MAX_JTI_PER_ISSUER)
            .for_each(|i| {
                let atproto = &atproto;
                let method = &method;
                let signing = &signing;
                async move {
                    let claims = serde_json::json!({
                        "iss": SQUID, "aud": KNOT, "exp": 1_001, "iat": 999,
                        "jti": format!("nonce-{i}"), "lxm": METHOD,
                    });
                    atproto
                        .verify_service_jwt(&mint(signing, &claims), method)
                        .await
                        .unwrap();
                }
            })
            .await;

        let hogged = serde_json::json!({
            "iss": SQUID, "aud": KNOT, "exp": 1_001, "iat": 999,
            "jti": "nonce-over-budget", "lxm": METHOD,
        });
        let error = atproto
            .verify_service_jwt(&mint(&signing, &hogged), &method)
            .await
            .unwrap_err();
        assert!(
            matches!(error, AtprotoError::ReplayShareExhausted { .. }),
            "issuer past its share fails closed, got {error:?}"
        );

        let bystander = serde_json::json!({
            "iss": "did:web:bystander.oyster.cafe", "aud": KNOT, "exp": 1_001, "iat": 999,
            "jti": "nonce-bystander", "lxm": METHOD,
        });
        atproto
            .verify_service_jwt(&mint(&signing, &bystander), &method)
            .await
            .expect("unrelated issuer is unaffected by the hog");

        atproto.clock.advance(Duration::from_secs(62));
        let after_expiry = serde_json::json!({
            "iss": SQUID, "aud": KNOT, "exp": 1_100, "iat": 1_050,
            "jti": "nonce-after-expiry", "lxm": METHOD,
        });
        atproto
            .verify_service_jwt(&mint(&signing, &after_expiry), &method)
            .await
            .expect("hog recovers once its nonces expire from the store");
    }

    #[tokio::test]
    async fn the_identity_cache_stays_bounded_and_retains_hot_entries() {
        let signing = signer(9);
        let hot_hits = Arc::new(AtomicUsize::new(0));
        let sentinel_hits = Arc::new(AtomicUsize::new(0));
        let hot = hot_hits.clone();
        let sentinel = sentinel_hits.clone();
        let http = FakeHttp::new(move |request| {
            let host = request.url.host_str().unwrap().to_string();
            if host == "hot.oyster.cafe" {
                hot.fetch_add(1, Ordering::SeqCst);
            }
            // yeah so what
            if host == "sentinel.oyster.cafe" {
                sentinel.fetch_add(1, Ordering::SeqCst);
            }
            let id = format!("did:web:{host}");
            Ok(ok(did_doc(DocSpec {
                id: &id,
                signing: &signing,
                handle: "nel.pet",
                pds: "https://pds.oyster.cafe",
                method: MethodKind::Multikey,
            })))
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let hot_did = did("did:web:hot.oyster.cafe");
        stream::iter(0..64)
            .for_each(|_| {
                let atproto = &atproto;
                let hot_did = hot_did.clone();
                async move {
                    atproto.resolve_identity(&hot_did).await.unwrap();
                }
            })
            .await;
        stream::iter(0..MAX_IDENTITY_CACHE * 2)
            .for_each(|i| {
                let atproto = &atproto;
                let hot_did = hot_did.clone();
                async move {
                    let filler = AccountDid::new(format!("did:web:c{i}.oyster.cafe")).unwrap();
                    atproto.resolve_identity(&filler).await.unwrap();
                    if i.is_multiple_of(8) {
                        atproto.resolve_identity(&hot_did).await.unwrap();
                    }
                }
            })
            .await;
        atproto.identities.run_pending_tasks().await;
        assert!(
            atproto.identities.entry_count().get() <= MAX_IDENTITY_CACHE as u64,
            "cache stays bounded under a cold-DID flood"
        );
        let before = hot_hits.load(Ordering::SeqCst);
        atproto.resolve_identity(&hot_did).await.unwrap();
        assert_eq!(
            hot_hits.load(Ordering::SeqCst),
            before,
            "frequently resolved identity is retained through the flood"
        );
        let novel = did("did:web:sentinel.oyster.cafe");
        atproto.resolve_identity(&novel).await.unwrap();
        atproto.resolve_identity(&novel).await.unwrap();
        assert_eq!(
            sentinel_hits.load(Ordering::SeqCst),
            1,
            "saturated cache still admits a new entry and reuses it without a re-fetch"
        );
        atproto.identities.run_pending_tasks().await;
        assert!(
            atproto.identities.entry_count().get() <= MAX_IDENTITY_CACHE as u64,
            "cache stays bounded after eviction"
        );
    }

    struct GatedHttp {
        status: StatusCode,
        body: Bytes,
        calls: Arc<AtomicUsize>,
        gate: Arc<tokio::sync::Notify>,
    }

    impl HttpTransport for GatedHttp {
        fn execute(&self, _request: HttpRequest) -> knot_runtime::HttpFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let gate = self.gate.clone();
            let response = status(self.status, self.body.clone());
            Box::pin(async move {
                gate.notified().await;
                Ok(response)
            })
        }
    }

    async fn gated_wave(
        st: StatusCode,
        body: Bytes,
    ) -> (Vec<Result<Identity, AtprotoError>>, usize) {
        let calls = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Notify::new());
        let http = GatedHttp {
            status: st,
            body,
            calls: calls.clone(),
            gate: gate.clone(),
        };
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let subject = did(SQUID);
        let resolves =
            futures::future::join_all((0..2000).map(|_| atproto.resolve_identity(&subject)));
        let releaser = async {
            stream::iter(0..16)
                .for_each(|_| async {
                    tokio::task::yield_now().await;
                })
                .await;
            gate.notify_one();
        };
        let (results, ()) = futures::join!(resolves, releaser);
        (results, calls.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn concurrent_cold_resolves_coalesce_into_one_fetch() {
        let (results, calls) = gated_wave(StatusCode::OK, squid_doc(&signer(9))).await;
        assert_eq!(results.len(), 2000);
        assert!(results.iter().all(|outcome| outcome.is_ok()));
        assert_eq!(
            calls, 1,
            "2000 concurrent resolves of one cold DID issue exactly one outbound fetch"
        );
    }

    #[tokio::test]
    async fn a_concurrent_429_wave_gets_the_real_error_and_never_poisons_the_cache() {
        let (results, calls) = gated_wave(StatusCode::TOO_MANY_REQUESTS, Bytes::new()).await;
        assert_eq!(calls, 1, "failing wave coalesces into one outbound fetch");
        assert!(
            results.iter().all(|outcome| matches!(
                outcome,
                Err(AtprotoError::Resolve(ResolveError::Status { status })) if status.get() == 429
            )),
            "every caller in the wave receives a real 429, never a poisoned RecentlyFailed"
        );
    }

    #[tokio::test]
    async fn every_caller_in_a_coalesced_404_wave_learns_the_account_is_missing() {
        let (results, calls) = gated_wave(StatusCode::NOT_FOUND, Bytes::new()).await;
        assert_eq!(calls, 1, "404 wave coalesces into one outbound fetch");
        let gone = results
            .iter()
            .filter(|outcome| {
                matches!(
                    outcome,
                    Err(AtprotoError::Resolve(ResolveError::Gone { status, .. })) if status.get() == 404
                )
            })
            .count();
        assert_eq!(
            gone, 2000,
            "coalescing mustn't decide which callers learn the account is missing, since the \
             callers served from the cache act on the answer the same way"
        );
    }

    #[tokio::test]
    async fn a_prepared_plc_operation_is_posted_and_a_rejection_is_typed() {
        let prepared = prepare_repo_did(
            &runtime_signer(11),
            &knot_types::KnotServiceUrl::new("https://knot.nel.pet").unwrap(),
            &repo_nonce(111),
        )
        .unwrap();
        let expected_path = format!("/{}", prepared.did.as_str());
        let http = FakeHttp::new(move |request| {
            assert_eq!(request.method, http::Method::POST);
            assert_eq!(request.url.path(), expected_path);
            assert!(request.body.as_ref().is_some_and(|body| !body.is_empty()));
            Ok(ok(Bytes::new()))
        });
        Atproto::new(http, clock(), knot_did(KNOT), plc())
            .submit_plc_operation(&prepared)
            .await
            .unwrap();

        let rejected = prepare_repo_did(
            &runtime_signer(12),
            &knot_types::KnotServiceUrl::new("https://knot.nel.pet").unwrap(),
            &repo_nonce(112),
        )
        .unwrap();
        let http = FakeHttp::new(move |_| Ok(status(StatusCode::BAD_REQUEST, Bytes::new())));
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        assert!(matches!(
            atproto.submit_plc_operation(&rejected).await,
            Err(AtprotoError::PlcSubmit { status, .. }) if status.get() == 400
        ));
    }

    #[tokio::test]
    async fn a_did_web_document_verification_covers_present_absent_and_missing() {
        let signing = signer(9);
        let doc = signing.clone();
        let http = FakeHttp::new(move |request| {
            assert_eq!(
                request.url.as_str(),
                "https://limpet.olaren.dev/.well-known/did.json"
            );
            Ok(ok(did_doc(DocSpec {
                id: "did:web:limpet.olaren.dev",
                signing: &doc,
                handle: "nel.pet",
                pds: "https://pds.oyster.cafe",
                method: MethodKind::Multikey,
            })))
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        atproto
            .verify_did_web_publishes_key(
                &repo_did("did:web:limpet.olaren.dev"),
                &PublicKeyBytes::from_bytes(sec1(&signing)),
            )
            .await
            .unwrap();

        let published = signer(9);
        let http = FakeHttp::new(move |_| {
            Ok(ok(did_doc(DocSpec {
                id: "did:web:limpet.olaren.dev",
                signing: &published,
                handle: "nel.pet",
                pds: "https://pds.oyster.cafe",
                method: MethodKind::Multikey,
            })))
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let error = atproto
            .verify_did_web_publishes_key(
                &repo_did("did:web:limpet.olaren.dev"),
                &PublicKeyBytes::from_bytes(sec1(&signer(3))),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AtprotoError::Resolve(ResolveError::ExpectedKeyAbsent { .. })
        ));

        let http = FakeHttp::new(|_| Ok(status(StatusCode::NOT_FOUND, Bytes::new())));
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let error = atproto
            .verify_did_web_publishes_key(
                &repo_did("did:web:limpet.olaren.dev"),
                &PublicKeyBytes::from_bytes(vec![1, 2, 3]),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AtprotoError::Resolve(ResolveError::Status { status }) if status.get() == 404
        ));
    }

    #[tokio::test]
    async fn a_pointer_record_is_published_to_the_subjects_pds_over_service_auth() {
        let doc_signer = signer(9);
        let knot_key = runtime_signer(21);
        let knot_public = knot_runtime::Signer::public_key(&knot_key);
        let http = FakeHttp::new(move |request| {
            if request.url.host_str() == Some("plc.directory") {
                return Ok(ok(squid_doc(&doc_signer)));
            }
            assert_eq!(request.method, http::Method::POST);
            assert_eq!(
                request.url.as_str(),
                "https://pds.oyster.cafe/xrpc/com.atproto.repo.putRecord"
            );
            let bearer = request
                .headers
                .get(http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .expect("request includes a bearer service token");
            let parsed = knot_types::service_auth::parse_jwt(bearer).unwrap();
            assert_eq!(parsed.claims().iss.as_str(), KNOT);
            assert_eq!(parsed.claims().aud.as_str(), "did:web:pds.oyster.cafe");
            assert_eq!(
                parsed.claims().lxm.as_ref().unwrap().as_str(),
                "com.atproto.repo.putRecord"
            );
            assert!(parsed.claims().jti.is_some());
            let key = knot_types::service_auth::PublicKey::from_k256_bytes(knot_public.as_bytes())
                .unwrap();
            knot_types::service_auth::verify_signature(&parsed, &key)
                .expect("token is signed by the knot key");
            let body: serde_json::Value =
                serde_json::from_slice(request.body.as_ref().unwrap()).unwrap();
            assert_eq!(body["repo"], SQUID);
            assert_eq!(body["collection"], "sh.tangled.knot.member");
            assert_eq!(body["rkey"], POINTER_RKEY);
            assert_eq!(body["record"]["$type"], "sh.tangled.knot.member");
            assert_eq!(body["record"]["subject"], "did:plc:lyna");
            assert_eq!(body["record"]["domain"], "knot.nel.pet");
            let receipt = serde_json::json!({
                "uri": format!("at://{SQUID}/sh.tangled.knot.member/{POINTER_RKEY}"),
                "cid": POINTER_CID,
            });
            Ok(ok(Bytes::from(serde_json::to_vec(&receipt).unwrap())))
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let rkey = Rkey::new_owned(POINTER_RKEY).unwrap();
        let receipt = atproto
            .publish_pointer(
                &ServiceAuth::new(&knot_key, &entropy(31)),
                &did(SQUID),
                &rkey,
                &member_pointer(),
            )
            .await
            .unwrap();
        assert_eq!(
            receipt.uri.as_str(),
            format!("at://{SQUID}/sh.tangled.knot.member/{POINTER_RKEY}")
        );
        assert_eq!(receipt.cid.as_str(), POINTER_CID);
    }

    async fn rejected_put_record(st: StatusCode) -> AtprotoError {
        let doc_signer = signer(9);
        let http = FakeHttp::new(move |request| {
            if request.url.host_str() == Some("plc.directory") {
                return Ok(ok(squid_doc(&doc_signer)));
            }
            Ok(status(st, Bytes::new()))
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let rkey = Rkey::new_owned(POINTER_RKEY).unwrap();
        atproto
            .publish_pointer(
                &ServiceAuth::new(&runtime_signer(22), &entropy(31)),
                &did(SQUID),
                &rkey,
                &member_pointer(),
            )
            .await
            .unwrap_err()
    }

    #[tokio::test]
    async fn a_rejected_put_record_is_a_typed_error_and_transient_only_on_server_failure() {
        let server_failure = rejected_put_record(StatusCode::BAD_GATEWAY).await;
        assert!(matches!(
            server_failure,
            AtprotoError::PutRecord { status, .. } if status.get() == 502
        ));
        assert!(server_failure.is_transient());

        let rate_limited = rejected_put_record(StatusCode::TOO_MANY_REQUESTS).await;
        assert!(matches!(
            rate_limited,
            AtprotoError::PutRecord { status, .. } if status.get() == 429
        ));
        assert!(rate_limited.is_transient());

        let client_failure = rejected_put_record(StatusCode::BAD_REQUEST).await;
        assert!(matches!(
            client_failure,
            AtprotoError::PutRecord { status, .. } if status.get() == 400
        ));
        assert!(!client_failure.is_transient());
    }

    #[tokio::test]
    async fn a_malformed_put_record_receipt_is_a_typed_error() {
        let doc_signer = signer(9);
        let http = FakeHttp::new(move |request| {
            if request.url.host_str() == Some("plc.directory") {
                return Ok(ok(squid_doc(&doc_signer)));
            }
            Ok(ok(Bytes::from_static(b"not json")))
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let rkey = Rkey::new_owned(POINTER_RKEY).unwrap();
        let error = atproto
            .publish_pointer(
                &ServiceAuth::new(&runtime_signer(23), &entropy(31)),
                &did(SQUID),
                &rkey,
                &member_pointer(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, AtprotoError::MalformedReceipt(_)));
    }

    #[tokio::test]
    async fn a_pointer_to_an_insecure_pds_endpoint_is_refused() {
        let doc_signer = signer(9);
        let http = FakeHttp::new(move |request| {
            assert_eq!(
                request.url.host_str(),
                Some("plc.directory"),
                "no request may reach the insecure PDS"
            );
            Ok(ok(did_doc(DocSpec {
                id: SQUID,
                signing: &doc_signer,
                handle: "nel.pet",
                pds: "http://pds.oyster.cafe",
                method: MethodKind::Multikey,
            })))
        });
        let atproto = Atproto::new(http, clock(), knot_did(KNOT), plc());
        let rkey = Rkey::new_owned(POINTER_RKEY).unwrap();
        let error = atproto
            .publish_pointer(
                &ServiceAuth::new(&runtime_signer(24), &entropy(31)),
                &did(SQUID),
                &rkey,
                &member_pointer(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AtprotoError::Resolve(ResolveError::InsecureScheme { .. })
        ));
    }
}
