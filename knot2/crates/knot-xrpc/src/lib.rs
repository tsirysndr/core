mod blocklist;
mod body;
mod branches;
mod cob;
mod collaborators;
mod error;
mod events;
mod forks;
pub mod legacy_admin;
mod lfs;
mod lists;
mod locks;
mod members;
mod merge;
mod patchtext;
mod query;
mod reads;
mod receive;
mod repos;
mod reservations;
mod service;
mod sniff;
mod wire;

#[cfg(test)]
mod tests;

pub use error::XrpcError;
pub use knot_pack::MaxWireBytes;
pub use knot_postreceive::LanguagesPushBudget;
pub use knot_resource::{
    Burst, GlobalInflight, LimitConfig, PerPeerInflight, PreAuthLimiter, RateLimit, RefillMicros,
};
pub use lfs::LfsWeb;
pub use locks::CobLocks;
pub use merge::Committer;
pub use receive::advertiser as receive_advertiser;
pub use reservations::{GlobalQuota, PerActorQuota, ReservationTtl, Reservations};

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, FromRequestParts, MatchedPath, Request, State};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use http::request::Parts;
use http::{HeaderMap, HeaderValue, StatusCode, header::AUTHORIZATION};
use serde::de::DeserializeOwned;
use serde_json::json;

use knot_atproto::{Atproto, AtprotoError, ServiceJwt};
use knot_events::{EventLog, SubscriberGate};
pub use knot_git::ArchiveLimit;
use knot_git::Layout;
use knot_index::{Index, Resolved};
use knot_maintenance::MaintenanceHandle;
use knot_resource::Slots;
use knot_runtime::{Clock, Entropy, HttpTransport};
use knot_secrets::SealedStore;
use knot_types::{
    AccountDid, AdmissionPolicy, AppviewEndpoint, CiLogsAddr, ClonePath, KnotHostname, KnotId,
    KnotServiceUrl, Nsid, OwnerDid, OwnerRef, RepoDid, UnixSeconds,
};

use base64::Engine;
use knot_pack::SocketPeer;
use knot_resource::{AdmitGuard, Refusal};

pub(crate) const PUSH_NSID: &str = "sh.tangled.repo.push";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReadBudget {
    Within(Duration),
    Unbounded,
}

impl ReadBudget {
    pub fn deadline(self) -> Option<Instant> {
        match self {
            ReadBudget::Within(budget) => Some(Instant::now() + budget),
            ReadBudget::Unbounded => None,
        }
    }
}

// `XrpcState` keeps a bunch of these side by side,
// some usize & some u64.
// Within each group every one of them typechecked in every other one's slot.
knot_types::scalar_newtype! {
    pub struct BodyLimit(usize);
    pub struct PatchLimit(usize);
    pub struct PatchDecompressedLimit(u64);
    pub struct ResponseLimit(usize);
    pub struct ForkPackLimit(u64);
    pub struct TreeReadBudget(ReadBudget);
    pub struct BlobReadBudget(ReadBudget);
    pub struct LanguagesReadBudget(ReadBudget);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteLimits {
    pub body: BodyLimit,
    pub patch: PatchLimit,
    pub patch_decompressed: PatchDecompressedLimit,
    pub response: ResponseLimit,
    pub archive: ArchiveLimit,
    pub fork_pack: ForkPackLimit,
    pub pack: MaxWireBytes,
}

impl Default for ByteLimits {
    fn default() -> Self {
        Self {
            body: BodyLimit::new(64 * 1024),
            patch: PatchLimit::new(16 * 1024 * 1024),
            patch_decompressed: PatchDecompressedLimit::new(128 * 1024 * 1024),
            response: ResponseLimit::new(5 * 1024 * 1024),
            archive: ArchiveLimit::default(),
            fork_pack: ForkPackLimit::new(1024 * 1024 * 1024),
            pack: MaxWireBytes::new(8 * 1024 * 1024 * 1024),
        }
    }
}

const BINARY_RESPONSE_SHARE: u64 = 4;
const BINARY_WIRE_COPIES: u64 = 3;

impl ByteLimits {
    pub fn binary_patch(self) -> knot_git::BinaryBudget {
        knot_git::BinaryBudget::new(
            self.response.get() as u64 / BINARY_RESPONSE_SHARE / BINARY_WIRE_COPIES,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budgets {
    pub tree_last_commit: TreeReadBudget,
    pub blob_last_commit: BlobReadBudget,
    pub languages: LanguagesReadBudget,
    pub languages_push: LanguagesPushBudget,
}

impl Default for Budgets {
    fn default() -> Self {
        Self {
            tree_last_commit: TreeReadBudget::new(ReadBudget::Within(Duration::from_millis(300))),
            blob_last_commit: BlobReadBudget::new(ReadBudget::Within(Duration::from_millis(2_000))),
            languages: LanguagesReadBudget::new(ReadBudget::Within(Duration::from_millis(1_000))),
            languages_push: LanguagesPushBudget::new(Duration::from_millis(2_000)),
        }
    }
}

pub struct XrpcState<H, C> {
    pub layout: Layout,
    pub index: Arc<Index>,
    pub atproto: Arc<Atproto<H, C>>,
    pub secrets: Arc<SealedStore>,
    pub entropy: Arc<dyn Entropy>,
    pub admins: BTreeSet<AccountDid>,
    pub admission: AdmissionPolicy,
    pub knot_did: KnotId,
    pub knot_hostname: KnotHostname,
    pub ci_logs: Option<CiLogsAddr>,
    pub meta_path: PathBuf,
    pub knot_service_url: KnotServiceUrl,
    pub limiter: Arc<PreAuthLimiter>,
    pub cob_locks: Arc<CobLocks>,
    pub reservations: Arc<Reservations>,
    pub proxy_trust: knot_types::ProxyTrust,
    pub committer: Committer,
    pub byte_limits: ByteLimits,
    pub budgets: Budgets,
    pub git_http: Arc<dyn HttpTransport>,
    pub pack_limits: knot_pack::PackLimits,
    pub service_owner: AccountDid,
    pub events: Arc<EventLog<C>>,
    pub subscriber_gate: Arc<SubscriberGate>,
    pub maintenance: MaintenanceHandle,
    pub appview: AppviewEndpoint,
    pub slots: Slots,
    pub lfs: Option<LfsWeb>,
    pub catalog: Arc<knot_messages::Catalog>,
}

impl<H: HttpTransport, C: Clock> XrpcState<H, C> {
    pub fn now(&self) -> UnixSeconds {
        UnixSeconds::new((self.atproto.now().get() / 1_000_000) as i64)
    }

    pub(crate) fn knot_authority(&self) -> &str {
        self.knot_service_url.authority()
    }

    pub(crate) async fn authenticate(
        &self,
        headers: &HeaderMap,
        method: &Method,
    ) -> Result<AccountDid, XrpcError> {
        let token = bearer(headers)?;
        self.atproto
            .verify_service_jwt(&token, method.nsid())
            .await
            .map_err(map_verify_error)
    }

    pub(crate) async fn authenticate_push(
        &self,
        headers: &HeaderMap,
    ) -> Result<AccountDid, XrpcError> {
        let token = push_credential(headers)?;
        let method = Nsid::new_owned(PUSH_NSID).expect("push nsid is always a valid nsid");
        self.atproto
            .verify_service_jwt_guarded(
                &token,
                &method,
                knot_atproto::ReplayGuard::ReusableUntilExpiry,
            )
            .await
            .map_err(map_verify_error)
    }
}

fn map_verify_error(error: AtprotoError) -> XrpcError {
    if error.is_transient() {
        XrpcError::upstream_unavailable(error.to_string())
    } else {
        XrpcError::auth_required(error.to_string())
    }
}

pub(crate) struct Method(Nsid);

impl Method {
    fn nsid(&self) -> &Nsid {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn from_nsid(nsid: &str) -> Self {
        Self(Nsid::new_owned(nsid).expect("test route nsid parses"))
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Method {
    type Rejection = XrpcError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let matched = MatchedPath::from_request_parts(parts, state)
            .await
            .map_err(|_| XrpcError::internal("xrpc handler reached without a matched route"))?;
        let nsid = matched
            .as_str()
            .strip_prefix("/xrpc/")
            .ok_or_else(|| XrpcError::internal("xrpc route paths are prefixed with /xrpc/"))?;
        Nsid::new_owned(nsid)
            .map(Self)
            .map_err(|_| XrpcError::internal("route nsid is always a valid nsid"))
    }
}

pub fn router<H: HttpTransport, C: Clock>(state: Arc<XrpcState<H, C>>) -> Router {
    let merge_routes = Router::new()
        .route(merge::MERGE_ROUTE, post(merge::merge::<H, C>))
        .route(merge::MERGE_CHECK_ROUTE, post(merge::merge_check::<H, C>))
        .layer(DefaultBodyLimit::max(state.byte_limits.patch.get()));
    Router::new()
        .merge(merge_routes)
        .route(members::ADD_ROUTE, post(members::add_member::<H, C>))
        .route(members::REMOVE_ROUTE, post(members::remove_member::<H, C>))
        .route(blocklist::BAN_ROUTE, post(blocklist::ban::<H, C>))
        .route(blocklist::UNBAN_ROUTE, post(blocklist::unban::<H, C>))
        .route(
            collaborators::ADD_ROUTE,
            post(collaborators::add_collaborator::<H, C>),
        )
        .route(
            collaborators::REMOVE_ROUTE,
            post(collaborators::remove_collaborator::<H, C>),
        )
        .route(repos::CREATE_ROUTE, post(repos::create_repo::<H, C>))
        .route(repos::DELETE_ROUTE, post(repos::delete_repo::<H, C>))
        .route(repos::RENAME_ROUTE, post(repos::rename_repo::<H, C>))
        .route(repos::RESERVE_ROUTE, post(repos::reserve_key::<H, C>))
        .route(
            branches::SET_DEFAULT_ROUTE,
            post(branches::set_default_branch::<H, C>),
        )
        .route(
            branches::DELETE_ROUTE,
            post(branches::delete_branch::<H, C>),
        )
        .route(forks::SYNC_ROUTE, post(forks::fork_sync::<H, C>))
        .route(forks::HIDDEN_REF_ROUTE, post(forks::hidden_ref::<H, C>))
        .route(reads::TREE_ROUTE, get(reads::repo_tree::<H, C>))
        .route(reads::LOG_ROUTE, get(reads::repo_log::<H, C>))
        .route(reads::BRANCHES_ROUTE, get(reads::repo_branches::<H, C>))
        .route(reads::BRANCH_ROUTE, get(reads::repo_branch::<H, C>))
        .route(reads::TAGS_ROUTE, get(reads::repo_tags::<H, C>))
        .route(reads::TAG_ROUTE, get(reads::repo_tag::<H, C>))
        .route(reads::BLOB_ROUTE, get(reads::repo_blob::<H, C>))
        .route(reads::DIFF_ROUTE, get(reads::repo_diff::<H, C>))
        .route(reads::COMPARE_ROUTE, get(reads::repo_compare::<H, C>))
        .route(reads::ARCHIVE_ROUTE, get(reads::repo_archive::<H, C>))
        .route(reads::LANGUAGES_ROUTE, get(reads::repo_languages::<H, C>))
        .route(
            reads::GET_DEFAULT_BRANCH_ROUTE,
            get(reads::repo_get_default_branch::<H, C>),
        )
        .route(
            reads::DESCRIBE_REPO_ROUTE,
            get(reads::repo_describe_repo::<H, C>),
        )
        .route(reads::LIST_REFS_ROUTE, get(reads::git_list_refs::<H, C>))
        .route(reads::LIST_REPOS_ROUTE, get(reads::sync_list_repos::<H, C>))
        .route(lists::LIST_MEMBERS_ROUTE, get(lists::list_members::<H, C>))
        .route(
            lists::LIST_COLLABORATORS_ROUTE,
            get(lists::list_collaborators::<H, C>),
        )
        .route(service::VERSION_ROUTE, get(service::version))
        .route(service::OWNER_ROUTE, get(service::owner::<H, C>))
        .layer(DefaultBodyLimit::max(state.byte_limits.body.get()))
        .layer(from_fn_with_state(
            Arc::clone(&state),
            enforce_pre_auth_limit::<H, C>,
        ))
        .merge(lfs::routes::<H, C>())
        .merge(receive::routes::<H, C>())
        .route(service::HEALTH_ROUTE, get(service::health::<H, C>))
        .route(events::EVENTS_ROUTE, get(events::events::<H, C>))
        .with_state(state)
}

pub(crate) async fn enforce_pre_auth_limit<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    socket: SocketPeer,
    request: Request,
    next: Next,
) -> Response {
    let peer = state
        .proxy_trust
        .client_peer(request.headers(), socket.ip());
    match admit_pre_auth(&state, peer) {
        Ok(guard) => {
            let response = next.run(request).await;
            drop(guard);
            response
        }
        Err(error) => error.into_response(),
    }
}

pub(crate) fn admit_pre_auth<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    peer: Option<IpAddr>,
) -> Result<AdmitGuard, XrpcError> {
    state
        .limiter
        .admit(peer, state.atproto.now())
        .map_err(|refusal| match refusal {
            Refusal::RateLimited => {
                XrpcError::rate_limited("too many pre-authentication requests, retry shortly")
            }
            Refusal::Saturated => {
                XrpcError::overloaded("knot is shedding pre-authentication load, retry shortly")
            }
        })
}

pub(crate) const BASIC_CHALLENGE: HeaderValue = HeaderValue::from_static("Basic realm=\"knot\"");

fn strip_bearer(value: &str) -> Option<&str> {
    let (scheme, rest) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("Bearer").then_some(rest)
}

fn bearer(headers: &HeaderMap) -> Result<ServiceJwt, XrpcError> {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(strip_bearer)
        .map(str::trim)
        .and_then(|token| ServiceJwt::new(token).ok())
        .ok_or_else(|| XrpcError::auth_required("missing or malformed Bearer authorization header"))
}

pub(crate) struct BasicUser(String);

impl BasicUser {
    pub(crate) fn matches(&self, expected: &str) -> bool {
        self.0 == expected
    }
}

pub(crate) struct BasicPassword(String);

impl BasicPassword {
    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

pub(crate) struct BasicCredentials {
    pub(crate) user: BasicUser,
    pub(crate) password: BasicPassword,
}

pub(crate) fn basic_credentials(value: &str) -> Option<BasicCredentials> {
    let (scheme, rest) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(rest.trim())
        .ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (user, password) = text.split_once(':')?;
    Some(BasicCredentials {
        user: BasicUser(user.to_string()),
        password: BasicPassword(password.to_string()),
    })
}

fn strip_basic(value: &str) -> Option<String> {
    basic_credentials(value)
        .map(|credentials| credentials.password.0)
        .filter(|password| !password.is_empty())
}

fn push_credential(headers: &HeaderMap) -> Result<ServiceJwt, XrpcError> {
    let value = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| XrpcError::auth_required("missing authorization header"))?;
    strip_bearer(value)
        .map(str::trim)
        .map(str::to_string)
        .or_else(|| strip_basic(value))
        .and_then(|token| ServiceJwt::new(token).ok())
        .ok_or_else(|| {
            XrpcError::auth_required("authorization isn't a bearer token or basic credential")
        })
}

pub(crate) fn decode<T: DeserializeOwned>(body: &Bytes) -> Result<T, XrpcError> {
    serde_json::from_slice(body)
        .map_err(|error| XrpcError::invalid_request(format!("invalid request body: {error}")))
}

pub(crate) fn ok_empty() -> Response {
    (StatusCode::OK, Json(json!({}))).into_response()
}

pub(crate) fn current_owner<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    repo: &RepoDid,
) -> Option<OwnerDid> {
    match state.index.owner_of(repo) {
        Resolved::Ready(owner) => owner,
        Resolved::Warming => None,
    }
}

pub(crate) async fn fold_collaborators<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    repo: &RepoDid,
) {
    let index = Arc::clone(&state.index);
    let target = repo.clone();
    let _ = run_blocking(move || Ok(index.ensure_collaborators(&target))).await;
}

pub(crate) async fn authorize_push<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    actor: &AccountDid,
    repo: &RepoDid,
    denied: &str,
) -> Result<(), XrpcError> {
    fold_collaborators(state, repo).await;
    let acl = knot_acl::KnotAcl::new(&state.admins, state.admission, &state.index);
    if knot_acl::can_push(&acl, actor, repo).is_allowed() {
        Ok(())
    } else {
        Err(XrpcError::forbidden(denied))
    }
}

pub(crate) async fn authenticate_and_authorize_push<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    socket: SocketPeer,
    headers: &HeaderMap,
    repo: &RepoDid,
    denied: &str,
) -> Result<AccountDid, XrpcError> {
    let peer = state.proxy_trust.client_peer(headers, socket.ip());
    let guard = admit_pre_auth(state, peer)?;
    let actor = state.authenticate_push(headers).await?;
    guard.refund();
    authorize_push(state, &actor, repo, denied).await?;
    Ok(actor)
}

pub(crate) async fn run_blocking<T, F>(task: F) -> Result<T, XrpcError>
where
    F: FnOnce() -> Result<T, XrpcError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(task).await {
        Ok(result) => result,
        Err(_) => Err(XrpcError::internal("blocking task failed to complete")),
    }
}

#[derive(serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct OwnerSegment(String);

#[derive(serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct RepoNameSegment(String);

impl OwnerSegment {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl RepoNameSegment {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct RepoDidSegment(String);

impl RepoDidSegment {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct RepoPathParams {
    pub(crate) did: OwnerSegment,
    pub(crate) name: RepoNameSegment,
}

pub(crate) fn resolve_repo_did<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    segment: &RepoDidSegment,
) -> Result<RepoDid, XrpcError> {
    let raw = segment.as_str();
    let trimmed = raw.strip_suffix(".git").unwrap_or(raw);
    let did = RepoDid::new(trimmed).map_err(|_| XrpcError::not_found("repository not found"))?;
    match state.index.owner_of(&did) {
        Resolved::Ready(Some(_)) => Ok(did),
        Resolved::Ready(None) => Err(XrpcError::not_found("repository not found")),
        Resolved::Warming => Err(XrpcError::warming(
            "registry projection is still warming, retry shortly",
        )),
    }
}

pub(crate) async fn resolve_repo_named<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    owner: &OwnerSegment,
    name: &RepoNameSegment,
) -> Result<RepoDid, XrpcError> {
    let owner = resolve_owner_segment(state, owner).await?;
    let path = ClonePath::parse(name.as_str())
        .ok_or_else(|| XrpcError::not_found("repository not found"))?;
    match state.index.resolve_clone_path(&owner, &path) {
        Resolved::Ready(Some(did)) => Ok(did),
        Resolved::Ready(None) => Err(XrpcError::not_found("repository not found")),
        Resolved::Warming => Err(XrpcError::warming(
            "registry projection is still warming, retry shortly",
        )),
    }
}

async fn resolve_owner_segment<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    owner: &OwnerSegment,
) -> Result<OwnerDid, XrpcError> {
    let not_found = || XrpcError::not_found("repository not found");
    match OwnerRef::parse(owner.as_str()).ok_or_else(not_found)? {
        OwnerRef::Did(did) => Ok(did),
        OwnerRef::Handle(handle) => state
            .atproto
            .resolve_handle_to_did(&handle)
            .await
            .map(OwnerDid::from)
            .map_err(|_| not_found()),
    }
}

#[cfg(test)]
mod credential_tests {
    use super::push_credential;
    use base64::Engine;
    use http::{HeaderMap, HeaderValue, header::AUTHORIZATION};

    fn with(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        headers
    }

    fn basic(user_pass: &str) -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(user_pass)
        )
    }

    #[test]
    fn a_bearer_token_is_taken_verbatim() {
        assert_eq!(
            push_credential(&with("Bearer jwt.abc.def"))
                .unwrap()
                .as_str(),
            "jwt.abc.def"
        );
        assert_eq!(
            push_credential(&with("bearer   jwt.abc.def"))
                .unwrap()
                .as_str(),
            "jwt.abc.def"
        );
    }

    #[test]
    fn a_basic_credential_yields_the_password_after_the_first_colon() {
        assert_eq!(
            push_credential(&with(&basic("x-tangled-token:jwt.abc.def")))
                .unwrap()
                .as_str(),
            "jwt.abc.def",
            "RFC 7617 puts the token in the password half, so the username stays colon-free"
        );
    }

    #[test]
    fn malformed_or_empty_credentials_are_rejected() {
        assert!(push_credential(&HeaderMap::new()).is_err());
        assert!(push_credential(&with("Bearer    ")).is_err());
        assert!(push_credential(&with(&basic("x-tangled-token:"))).is_err());
        assert!(push_credential(&with(&basic("no-colon"))).is_err());
        assert!(push_credential(&with("Basic !!!not-base64")).is_err());
        assert!(push_credential(&with("Digest whatever")).is_err());
    }
}
