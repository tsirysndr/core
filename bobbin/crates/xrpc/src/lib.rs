use std::future::Future;
use std::sync::Arc;

use bobbin_resolver::{
    NormalizeRepoRefs, decode_canon_or_upgrade, normalize_record_fields, scrub_record_bytes,
    upgrade_wire_bytes,
};

use axum::{
    Router,
    body::Body,
    extract::{FromRequestParts, Query, RawQuery, State, rejection::QueryRejection},
    http::{
        HeaderMap, HeaderName, StatusCode,
        header::{
            ACCEPT_RANGES, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_ENCODING, CONTENT_LENGTH,
            CONTENT_RANGE, CONTENT_TYPE, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, IF_RANGE,
            LAST_MODIFIED, RANGE,
        },
        request::Parts,
    },
    response::{IntoResponse, Json, Response},
    routing::get,
};
use bobbin_edge_index::{
    Coverage, CoverageWatch, CursorParseError, EdgeItem, EdgePage, EdgeStore, IssueStateKind,
    PageCursor, PageLimit, PageToken, PullStatusKind, SortDir, StateIndex, StateKind,
};
use bobbin_knot_proxy::{KnotHost, KnotProxy, KnotProxyError, ProxyResponse, RepoSlug};
use bobbin_record_lru::RecordStore;
use bobbin_resolver::RepoIdResolver;
use bobbin_search::{
    SearchCursor, SearchError, SearchFilters, SearchHit, SearchOffset, SearchReader,
};
use bobbin_slingshot_client::{SlingshotClient, SlingshotError};
use bobbin_types::ids::{EdgeKey, SubjectRef, nsid_static};
use bobbin_types::knot_acl::{KnotOwnedSource, decode_knot_owned_source, knot_did_host};
use bobbin_types::record::RecordBody;
use bobbin_types::search::SearchableRecord;
use bobbin_types::sh_tangled::actor::profile::{Profile, ProfileGetRecordOutput, ProfileRecord};
use bobbin_types::sh_tangled::feed::comment::{
    Comment as FeedComment, CommentRecord as FeedCommentRecord,
};
use bobbin_types::sh_tangled::feed::reaction::{Reaction, ReactionRecord};
use bobbin_types::sh_tangled::feed::star::{Star, StarRecord};
use bobbin_types::sh_tangled::git::ref_update::{RefUpdate, RefUpdateRecord};
use bobbin_types::sh_tangled::graph::follow::{Follow, FollowRecord};
use bobbin_types::sh_tangled::graph::vouch::{Vouch, VouchRecord};
use bobbin_types::sh_tangled::knot::member::{
    Member as KnotMember, MemberRecord as KnotMemberRecord,
};
use bobbin_types::sh_tangled::knot::{Knot, KnotRecord};
use bobbin_types::sh_tangled::label::definition::{
    Definition as LabelDefinition, DefinitionRecord as LabelDefinitionRecord,
};
use bobbin_types::sh_tangled::label::op::{Op as LabelOp, OpRecord as LabelOpRecord};
use bobbin_types::sh_tangled::pipeline::status::{
    Status as PipelineStatus, StatusRecord as PipelineStatusRecord,
};
use bobbin_types::sh_tangled::pipeline::{Pipeline, PipelineRecord};
use bobbin_types::sh_tangled::public_key::{PublicKey, PublicKeyRecord};
use bobbin_types::sh_tangled::repo::artifact::{Artifact, ArtifactRecord};
use bobbin_types::sh_tangled::repo::collaborator::{Collaborator, CollaboratorRecord};
use bobbin_types::sh_tangled::repo::issue::state::{
    State as IssueState, StateRecord as IssueStateRecord,
};
use bobbin_types::sh_tangled::repo::issue::{Issue, IssueGetRecordOutput, IssueRecord};
use bobbin_types::sh_tangled::repo::pull::status::{
    Status as PullStatus, StatusRecord as PullStatusRecord,
};
use bobbin_types::sh_tangled::repo::pull::{Pull, PullGetRecordOutput, PullRecord};
use bobbin_types::sh_tangled::repo::{Repo, RepoGetRecordOutput, RepoRecord};
use bobbin_types::sh_tangled::spindle::member::{
    Member as SpindleMember, MemberRecord as SpindleMemberRecord,
};
use bobbin_types::sh_tangled::spindle::{Spindle, SpindleRecord};
use bobbin_types::sh_tangled::string::{TangledString, TangledStringRecord};
use futures::Stream;
use futures::stream::{self, StreamExt, TryStreamExt};
use jacquard_common::types::did::Did;
use jacquard_common::types::ident::AtIdentifier;
use jacquard_common::types::nsid::Nsid;
use jacquard_common::types::recordkey::Rkey;
use jacquard_common::types::string::{AtUri, Cid};
use jacquard_common::xrpc::XrpcResp;
use jacquard_common::{DefaultStr, IntoStatic};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::time::Duration;
use thiserror::Error;
use url::form_urlencoded;

use tower_http::classify::ServerErrorsFailureClass;
use tower_http::trace::{DefaultMakeSpan, OnFailure, OnResponse, TraceLayer};
use tracing::{Level, Span};

mod backpressure;
mod client_address;
mod filter;

pub use backpressure::{
    HeavyLimiter, HeavyPermit, MaxInFlight, PerRequestAnonBytes, PressureVerdict, ReservedFloor,
};
use client_address::X_FORWARDED_FOR;
pub use client_address::{ClientAddress, SocketPeer};
use filter::{IssueFilter, ListFilter, NoFilter, PullFilter};
use trusted_proxies::TrustedProxies;

const DEFAULT_LIMIT: u32 = 50;
const FETCH_CONCURRENCY: usize = 8;

#[derive(Clone)]
pub struct AppState {
    pub records: Arc<dyn RecordStore>,
    pub slingshot: SlingshotClient,
    pub edges: Arc<EdgeStore>,
    pub issue_states: Arc<StateIndex<IssueStateKind>>,
    pub pull_statuses: Arc<StateIndex<PullStatusKind>>,
    pub coverage: Arc<CoverageWatch>,
    pub knots: Arc<KnotProxy>,
    pub search: Arc<dyn SearchReader>,
    pub resolver: Arc<RepoIdResolver>,
    pub limiter: Option<Arc<HeavyLimiter>>,
    pub client_address: Arc<ClientAddress>,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        records: Arc<dyn RecordStore>,
        slingshot: SlingshotClient,
        edges: Arc<EdgeStore>,
        issue_states: Arc<StateIndex<IssueStateKind>>,
        pull_statuses: Arc<StateIndex<PullStatusKind>>,
        coverage: Arc<CoverageWatch>,
        knots: Arc<KnotProxy>,
        search: Arc<dyn SearchReader>,
        resolver: Arc<RepoIdResolver>,
    ) -> Self {
        Self {
            records,
            slingshot,
            edges,
            issue_states,
            pull_statuses,
            coverage,
            knots,
            search,
            resolver,
            limiter: None,
            client_address: Arc::new(ClientAddress::default()),
        }
    }

    pub fn with_limiter(mut self, limiter: Option<Arc<HeavyLimiter>>) -> Self {
        self.limiter = limiter;
        self
    }

    pub fn with_proxies(mut self, proxies: TrustedProxies) -> Self {
        self.client_address = Arc::new(ClientAddress::new(proxies));
        self
    }

    fn heavy_permit(&self) -> Result<Option<HeavyPermit>, XrpcError> {
        self.limiter.as_ref().map(|l| l.try_enter()).transpose()
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/xrpc/sh.tangled.repo.getRepo", get(get_repo))
        .route("/xrpc/sh.tangled.repo.getRepos", get(get_repos))
        .route(
            "/xrpc/sh.tangled.repo.getRepoByRepoDid",
            get(get_repo_by_repo_did),
        )
        .route("/xrpc/sh.tangled.actor.getProfile", get(get_profile))
        .route("/xrpc/sh.tangled.actor.getProfiles", get(get_profiles))
        .route("/xrpc/sh.tangled.repo.getIssue", get(get_issue))
        .route("/xrpc/sh.tangled.repo.getIssues", get(get_issues))
        .route("/xrpc/sh.tangled.repo.getPull", get(get_pull))
        .route("/xrpc/sh.tangled.repo.getPulls", get(get_pulls))
        .route("/xrpc/sh.tangled.feed.listStars", get(list_stars))
        .route("/xrpc/sh.tangled.feed.countStars", get(count_stars))
        .route("/xrpc/sh.tangled.graph.listFollows", get(list_follows))
        .route("/xrpc/sh.tangled.graph.countFollows", get(count_follows))
        .route("/xrpc/sh.tangled.repo.listIssues", get(list_issues))
        .route("/xrpc/sh.tangled.repo.countIssues", get(count_issues))
        .route("/xrpc/sh.tangled.repo.listPulls", get(list_pulls))
        .route("/xrpc/sh.tangled.repo.countPulls", get(count_pulls))
        .route(
            "/xrpc/sh.tangled.feed.listComments",
            get(list_feed_comments),
        )
        .route(
            "/xrpc/sh.tangled.feed.countComments",
            get(count_feed_comments),
        )
        .route("/xrpc/sh.tangled.feed.listReactions", get(list_reactions))
        .route("/xrpc/sh.tangled.feed.countReactions", get(count_reactions))
        .route("/xrpc/sh.tangled.git.listRefUpdates", get(list_ref_updates))
        .route(
            "/xrpc/sh.tangled.git.countRefUpdates",
            get(count_ref_updates),
        )
        .route(
            "/xrpc/sh.tangled.repo.listCollaborators",
            get(list_collaborators),
        )
        .route(
            "/xrpc/sh.tangled.repo.countCollaborators",
            get(count_collaborators),
        )
        .route(
            "/xrpc/sh.tangled.repo.issue.listStates",
            get(list_issue_states),
        )
        .route(
            "/xrpc/sh.tangled.repo.issue.countStates",
            get(count_issue_states),
        )
        .route(
            "/xrpc/sh.tangled.repo.pull.listStatuses",
            get(list_pull_statuses),
        )
        .route(
            "/xrpc/sh.tangled.repo.pull.countStatuses",
            get(count_pull_statuses),
        )
        .route("/xrpc/sh.tangled.repo.listRepos", get(list_repos))
        .route("/xrpc/sh.tangled.repo.countRepos", get(count_repos))
        .route("/xrpc/sh.tangled.knot.listKnots", get(list_knots))
        .route("/xrpc/sh.tangled.knot.countKnots", get(count_knots))
        .route("/xrpc/sh.tangled.spindle.listSpindles", get(list_spindles))
        .route(
            "/xrpc/sh.tangled.spindle.countSpindles",
            get(count_spindles),
        )
        .route("/xrpc/sh.tangled.publicKey.listKeys", get(list_public_keys))
        .route(
            "/xrpc/sh.tangled.publicKey.countKeys",
            get(count_public_keys),
        )
        .route("/xrpc/sh.tangled.graph.listVouches", get(list_vouches))
        .route("/xrpc/sh.tangled.graph.countVouches", get(count_vouches))
        .route("/xrpc/sh.tangled.feed.listStarsBy", get(list_stars_by))
        .route("/xrpc/sh.tangled.feed.countStarsBy", get(count_stars_by))
        .route(
            "/xrpc/sh.tangled.feed.listReactionsBy",
            get(list_reactions_by),
        )
        .route(
            "/xrpc/sh.tangled.feed.countReactionsBy",
            get(count_reactions_by),
        )
        .route("/xrpc/sh.tangled.graph.listFollowsBy", get(list_follows_by))
        .route(
            "/xrpc/sh.tangled.graph.countFollowsBy",
            get(count_follows_by),
        )
        .route("/xrpc/sh.tangled.graph.listVouchesBy", get(list_vouches_by))
        .route(
            "/xrpc/sh.tangled.graph.countVouchesBy",
            get(count_vouches_by),
        )
        .route(
            "/xrpc/sh.tangled.git.listRefUpdatesBy",
            get(list_ref_updates_by),
        )
        .route(
            "/xrpc/sh.tangled.git.countRefUpdatesBy",
            get(count_ref_updates_by),
        )
        .route(
            "/xrpc/sh.tangled.knot.listMembersBy",
            get(list_knot_members_by),
        )
        .route(
            "/xrpc/sh.tangled.knot.countMembersBy",
            get(count_knot_members_by),
        )
        .route("/xrpc/sh.tangled.label.listOpsBy", get(list_label_ops_by))
        .route("/xrpc/sh.tangled.label.countOpsBy", get(count_label_ops_by))
        .route(
            "/xrpc/sh.tangled.pipeline.listPipelinesBy",
            get(list_pipelines_by),
        )
        .route(
            "/xrpc/sh.tangled.pipeline.countPipelinesBy",
            get(count_pipelines_by),
        )
        .route(
            "/xrpc/sh.tangled.pipeline.listStatusesBy",
            get(list_pipeline_statuses_by),
        )
        .route(
            "/xrpc/sh.tangled.pipeline.countStatusesBy",
            get(count_pipeline_statuses_by),
        )
        .route(
            "/xrpc/sh.tangled.repo.listArtifactsBy",
            get(list_artifacts_by),
        )
        .route(
            "/xrpc/sh.tangled.repo.countArtifactsBy",
            get(count_artifacts_by),
        )
        .route(
            "/xrpc/sh.tangled.repo.listCollaboratorsBy",
            get(list_collaborators_by),
        )
        .route(
            "/xrpc/sh.tangled.repo.countCollaboratorsBy",
            get(count_collaborators_by),
        )
        .route("/xrpc/sh.tangled.repo.listIssuesBy", get(list_issues_by))
        .route("/xrpc/sh.tangled.repo.countIssuesBy", get(count_issues_by))
        .route(
            "/xrpc/sh.tangled.feed.listCommentsBy",
            get(list_feed_comments_by),
        )
        .route(
            "/xrpc/sh.tangled.feed.countCommentsBy",
            get(count_feed_comments_by),
        )
        .route(
            "/xrpc/sh.tangled.repo.issue.listStatesBy",
            get(list_issue_states_by),
        )
        .route(
            "/xrpc/sh.tangled.repo.issue.countStatesBy",
            get(count_issue_states_by),
        )
        .route("/xrpc/sh.tangled.repo.listPullsBy", get(list_pulls_by))
        .route("/xrpc/sh.tangled.repo.countPullsBy", get(count_pulls_by))
        .route(
            "/xrpc/sh.tangled.repo.pull.listStatusesBy",
            get(list_pull_statuses_by),
        )
        .route(
            "/xrpc/sh.tangled.repo.pull.countStatusesBy",
            get(count_pull_statuses_by),
        )
        .route(
            "/xrpc/sh.tangled.spindle.listMembersBy",
            get(list_spindle_members_by),
        )
        .route(
            "/xrpc/sh.tangled.spindle.countMembersBy",
            get(count_spindle_members_by),
        )
        .route(
            "/xrpc/sh.tangled.label.listDefinitions",
            get(list_label_definitions),
        )
        .route(
            "/xrpc/sh.tangled.label.countDefinitions",
            get(count_label_definitions),
        )
        .route("/xrpc/sh.tangled.label.listOps", get(list_label_ops))
        .route("/xrpc/sh.tangled.label.countOps", get(count_label_ops))
        .route(
            "/xrpc/sh.tangled.pipeline.listPipelines",
            get(list_pipelines),
        )
        .route(
            "/xrpc/sh.tangled.pipeline.countPipelines",
            get(count_pipelines),
        )
        .route(
            "/xrpc/sh.tangled.pipeline.listStatuses",
            get(list_pipeline_statuses),
        )
        .route(
            "/xrpc/sh.tangled.pipeline.countStatuses",
            get(count_pipeline_statuses),
        )
        .route("/xrpc/sh.tangled.repo.listArtifacts", get(list_artifacts))
        .route("/xrpc/sh.tangled.repo.countArtifacts", get(count_artifacts))
        .route("/xrpc/sh.tangled.knot.listMembers", get(list_knot_members))
        .route(
            "/xrpc/sh.tangled.knot.countMembers",
            get(count_knot_members),
        )
        .route(
            "/xrpc/sh.tangled.spindle.listMembers",
            get(list_spindle_members),
        )
        .route(
            "/xrpc/sh.tangled.spindle.countMembers",
            get(count_spindle_members),
        )
        .route("/xrpc/sh.tangled.string.listStrings", get(list_strings))
        .route("/xrpc/sh.tangled.string.countStrings", get(count_strings))
        .route("/xrpc/sh.tangled.search.query", get(search_query))
        .route("/xrpc/sh.tangled.bobbin.getCoverage", get(get_coverage))
        .route(
            "/xrpc/com.bad-example.identity.resolveMiniDoc",
            get(resolve_mini_doc),
        )
        .merge(knot_proxied_routes())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_request(())
                .on_response(LatencyFreeTrace)
                .on_failure(LatencyFreeTrace),
        )
        .with_state(state)
}

#[derive(Clone, Copy, Debug)]
struct LatencyFreeTrace;

impl<B> OnResponse<B> for LatencyFreeTrace {
    fn on_response(self, response: &Response<B>, _latency: Duration, _span: &Span) {
        tracing::event!(
            target: "tower_http::trace::on_response",
            Level::INFO,
            status = response.status().as_u16(),
            "request completed",
        );
    }
}

impl OnFailure<ServerErrorsFailureClass> for LatencyFreeTrace {
    fn on_failure(&mut self, error: ServerErrorsFailureClass, _latency: Duration, _span: &Span) {
        tracing::event!(
            target: "tower_http::trace::on_failure",
            Level::WARN,
            error = %error,
            "request failed",
        );
    }
}

const REPO_PROXIED_NSIDS: &[&str] = &[
    "sh.tangled.repo.archive",
    "sh.tangled.repo.blob",
    "sh.tangled.repo.branch",
    "sh.tangled.repo.branches",
    "sh.tangled.repo.compare",
    "sh.tangled.repo.describeRepo",
    "sh.tangled.repo.diff",
    "sh.tangled.repo.getDefaultBranch",
    "sh.tangled.repo.languages",
    "sh.tangled.repo.listSecrets",
    "sh.tangled.repo.log",
    "sh.tangled.repo.tag",
    "sh.tangled.repo.tags",
    "sh.tangled.repo.tree",
];

const KNOT_PROXIED_NSIDS: &[&str] = &[
    "sh.tangled.owner",
    "sh.tangled.knot.version",
    "sh.tangled.knot.listKeys",
];

const PASSTHROUGH_HEADERS: &[&HeaderName] = &[
    &CONTENT_TYPE,
    &CONTENT_LENGTH,
    &CONTENT_ENCODING,
    &ETAG,
    &CACHE_CONTROL,
    &LAST_MODIFIED,
    &CONTENT_DISPOSITION,
    &ACCEPT_RANGES,
    &CONTENT_RANGE,
];

const FORWARDED_REQUEST_HEADERS: &[&HeaderName] =
    &[&RANGE, &IF_RANGE, &IF_NONE_MATCH, &IF_MODIFIED_SINCE];

const KNOT_HOST_PARAM: &str = "knot";
const REPO_PARAM: &str = "repo";

type ProxyParams = Vec<(String, String)>;

fn knot_proxied_routes() -> Router<AppState> {
    let with_repo = register_proxied(Router::new(), REPO_PROXIED_NSIDS, proxy_repo_handler);
    register_proxied(with_repo, KNOT_PROXIED_NSIDS, proxy_knot_handler)
}

fn register_proxied<H, Fut>(
    router: Router<AppState>,
    nsids: &[&'static str],
    handler: H,
) -> Router<AppState>
where
    H: Fn(AppState, HeaderMap, SocketPeer, ProxyParams, Nsid<DefaultStr>) -> Fut
        + Clone
        + Send
        + Sync
        + 'static,
    Fut: Future<Output = Result<Response, XrpcError>> + Send + 'static,
{
    nsids.iter().fold(router, |router, &nsid_lit| {
        let handler = handler.clone();
        let nsid = nsid_static(nsid_lit);
        router.route(
            &format!("/xrpc/{nsid_lit}"),
            get(
                move |State(state): State<AppState>,
                      headers: HeaderMap,
                      socket: SocketPeer,
                      Query(params): Query<ProxyParams>| {
                    handler(state, headers, socket, params, nsid.clone())
                },
            ),
        )
    })
}

#[derive(Clone, Debug)]
pub enum SubjectQuery {
    Did(Did<DefaultStr>),
    Uri(AtUri<DefaultStr>),
}

impl<'de> Deserialize<'de> for SubjectQuery {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        if let Ok(did) = Did::<DefaultStr>::new_owned(&raw) {
            return Ok(Self::Did(did));
        }
        AtUri::<DefaultStr>::new_owned(&raw)
            .map(Self::Uri)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExpectedNsid {
    canon: Nsid<DefaultStr>,
    aliases: &'static [&'static str],
}

const FEED_COMMENT_LEGACY_ALIASES: &[&str] = &[
    "sh.tangled.repo.issue.comment",
    "sh.tangled.repo.pull.comment",
];

fn aliases_for(nsid: &str) -> &'static [&'static str] {
    match nsid {
        "sh.tangled.feed.comment" => FEED_COMMENT_LEGACY_ALIASES,
        _ => &[],
    }
}

impl ExpectedNsid {
    pub fn new(nsid: Nsid<DefaultStr>) -> Self {
        let aliases = aliases_for(nsid.as_ref());
        Self {
            canon: nsid,
            aliases,
        }
    }

    pub fn from_static(s: &'static str) -> Self {
        let canon = nsid_static(s);
        let aliases = aliases_for(s);
        Self { canon, aliases }
    }

    pub fn as_nsid(&self) -> &Nsid<DefaultStr> {
        &self.canon
    }

    pub fn as_str(&self) -> &str {
        self.canon.as_ref()
    }

    fn accepts(&self, other: &str) -> bool {
        other == self.canon.as_ref() || self.aliases.contains(&other)
    }
}

#[derive(Debug, Deserialize)]
struct GetRepoQuery {
    repo: AtUri<DefaultStr>,
}

#[derive(Debug, Deserialize)]
struct GetRepoByRepoDidQuery {
    #[serde(rename = "repoDid")]
    repo_did: Did<DefaultStr>,
}

#[derive(Debug, Deserialize)]
struct GetProfileQuery {
    actor: AtUri<DefaultStr>,
}

#[derive(Debug, Deserialize)]
struct GetIssueQuery {
    issue: AtUri<DefaultStr>,
}

#[derive(Debug, Deserialize)]
struct GetPullQuery {
    pull: AtUri<DefaultStr>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Order {
    Asc,
    #[default]
    Desc,
}

impl From<Order> for SortDir {
    fn from(o: Order) -> Self {
        match o {
            Order::Asc => SortDir::Asc,
            Order::Desc => SortDir::Desc,
        }
    }
}

#[derive(Debug, Deserialize)]
struct TypedListQuery<F> {
    subject: SubjectQuery,
    cursor: Option<String>,
    limit: Option<u32>,
    #[serde(default)]
    order: Order,
    #[serde(flatten)]
    filter: F,
}

impl<F> TypedListQuery<F> {
    fn dir(&self) -> SortDir {
        self.order.into()
    }
}

#[derive(Debug, Deserialize)]
struct CountQuery {
    subject: SubjectQuery,
}

#[derive(Debug, Deserialize)]
struct SearchQueryParams {
    q: String,
    nsid: Option<Nsid<DefaultStr>>,
    author: Option<Did<DefaultStr>>,
    repo: Option<Did<DefaultStr>>,
    since: Option<String>,
    until: Option<String>,
    cursor: Option<String>,
    limit: Option<u32>,
}

pub struct XrpcQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for XrpcQuery<T>
where
    S: Send + Sync,
    T: serde::de::DeserializeOwned + Send + 'static,
{
    type Rejection = XrpcError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(t)| Self(t))
            .map_err(|rej: QueryRejection| XrpcError::InvalidParams(rej.body_text()))
    }
}

#[derive(Debug, Error)]
pub enum XrpcError {
    #[error("invalid request: {0}")]
    InvalidParams(String),
    #[error("record not found")]
    NotFound,
    #[error("upstream unavailable: {0}")]
    UpstreamUnavailable(String),
    #[error("upstream gone: {0}")]
    UpstreamGone(String),
    #[error("invalid record: {0}")]
    InvalidRecord(String),
    #[error("internal: {0}")]
    Internal(String),
    #[error("overloaded, shedding under memory pressure")]
    Overloaded,
}

impl XrpcError {
    pub fn overloaded() -> Self {
        Self::Overloaded
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    message: String,
}

impl IntoResponse for XrpcError {
    fn into_response(self) -> Response {
        let (status, error) = match &self {
            Self::InvalidParams(_) => (StatusCode::BAD_REQUEST, "InvalidRequest"),
            Self::NotFound => (StatusCode::NOT_FOUND, "RecordNotFound"),
            Self::UpstreamUnavailable(_) => (StatusCode::BAD_GATEWAY, "UpstreamFailed"),
            Self::UpstreamGone(_) => (StatusCode::BAD_GATEWAY, "UpstreamGone"),
            Self::InvalidRecord(_) => (StatusCode::BAD_GATEWAY, "InvalidRecord"),
            Self::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "InternalError"),
            Self::Overloaded => (StatusCode::SERVICE_UNAVAILABLE, "Overloaded"),
        };
        let body = ErrorBody {
            error,
            message: self.to_string(),
        };
        (status, Json(body)).into_response()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CoverageEnvelope {
    ready: bool,
    events_processed: u64,
    last_cursor: u64,
}

impl From<Coverage> for CoverageEnvelope {
    fn from(c: Coverage) -> Self {
        Self {
            ready: c.is_ready(),
            events_processed: c.events_processed(),
            last_cursor: c.last_cursor().raw(),
        }
    }
}

struct Deduped<T>(T);

impl<T: Serialize> Serialize for Deduped<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serde_json::to_value(&self.0)
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RecordView<V> {
    uri: AtUri<DefaultStr>,
    cid: Option<Cid<DefaultStr>>,
    value: V,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatefulItem<V> {
    #[serde(flatten)]
    view: RecordView<V>,
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    state_updated_at: Option<String>,
    comment_count: u64,
}

fn format_micros(micros: u64) -> String {
    let signed = i64::try_from(micros).ok();
    let rfc = signed
        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_micros)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Micros, true));
    rfc.unwrap_or_else(|| micros.to_string())
}

pub(crate) fn source_authority_did(source: &AtUri<DefaultStr>) -> Option<Did<DefaultStr>> {
    match source.authority() {
        AtIdentifier::Did(d) => Some(d.clone().into_static()),
        AtIdentifier::Handle(_) => None,
    }
}

fn enrich_issue_view(
    state: &AppState,
    view: RecordView<Issue<DefaultStr>>,
) -> StatefulItem<Issue<DefaultStr>> {
    let issue_author = source_authority_did(&view.uri);
    let repo_did = view.value.repo.clone();
    enrich_view(
        &state.edges,
        nsid_static("sh.tangled.feed.comment"),
        &state.issue_states,
        view,
        move |src| accept_state_source(src, issue_author.as_ref(), &repo_did),
    )
}

fn enrich_pull_view(
    state: &AppState,
    view: RecordView<Pull<DefaultStr>>,
) -> StatefulItem<Pull<DefaultStr>> {
    let pull_author = source_authority_did(&view.uri);
    let target_repo = view.value.target.repo.clone();
    enrich_view(
        &state.edges,
        nsid_static("sh.tangled.feed.comment"),
        &state.pull_statuses,
        view,
        move |src| accept_state_source(src, pull_author.as_ref(), &target_repo),
    )
}

pub(crate) fn accept_state_source(
    source: &AtUri<DefaultStr>,
    entity_author: Option<&Did<DefaultStr>>,
    repo_owner: &Did<DefaultStr>,
) -> bool {
    let Some(src) = source_authority_did(source) else {
        return false;
    };
    Some(&src) == entity_author || &src == repo_owner
}

fn enrich_view<V, K, F>(
    edges: &EdgeStore,
    comment_nsid: Nsid<DefaultStr>,
    states: &StateIndex<K>,
    view: RecordView<V>,
    accept: F,
) -> StatefulItem<V>
where
    K: StateKind + Default,
    F: Fn(&AtUri<DefaultStr>) -> bool,
{
    let comment_count = edges.count(&EdgeKey::new(
        comment_nsid,
        SubjectRef::Uri(view.uri.clone()),
    ));
    let (state, state_updated_at) = states
        .latest_by(&view.uri, accept)
        .map_or((K::default().wire(), None), |(kind, micros)| {
            (kind.wire(), Some(format_micros(micros)))
        });
    StatefulItem {
        view,
        state,
        state_updated_at,
        comment_count,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CountResponse {
    count: u64,
    distinct_authors: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchHitView {
    uri: AtUri<DefaultStr>,
    cid: Option<Cid<DefaultStr>>,
    nsid: Nsid<DefaultStr>,
    score: f32,
    value: SearchableRecord,
}

fn map_slingshot(err: SlingshotError) -> XrpcError {
    use SlingshotError as E;
    match err {
        E::NotFound => XrpcError::NotFound,
        e @ (E::Decode(_)
        | E::MissingField(_)
        | E::InvalidAtUri(_)
        | E::InvalidCid(_)
        | E::UriMismatch { .. }) => XrpcError::InvalidRecord(e.to_string()),
        e @ (E::Network(_)
        | E::Build(_)
        | E::Upstream(_)
        | E::BodyTooLarge { .. }
        | E::BadScheme(_)) => XrpcError::UpstreamUnavailable(e.to_string()),
    }
}

fn parse_uri(raw: &str) -> Result<AtUri<DefaultStr>, XrpcError> {
    AtUri::<DefaultStr>::new_owned(raw).map_err(|e| XrpcError::InvalidParams(format!("uri: {e}")))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubjectShape {
    BareDid,
    Collection(&'static str),
    BareDidOrOneOfCollections(&'static [&'static str]),
    OneOfCollections(&'static [&'static str]),
    AnyAtUri,
}

pub trait HasSubject {
    const SHAPE: SubjectShape;
}

pub trait MirrorOf {
    type Record: XrpcResp;
    const EDGE_KIND: &'static str;
    const SHAPE: SubjectShape;
}

pub struct StarBy;
pub struct ReactionBy;
pub struct FollowBy;
pub struct VouchBy;
pub struct RefUpdateBy;
pub struct KnotMemberBy;
pub struct LabelOpBy;
pub struct PipelineBy;
pub struct PipelineStatusBy;
pub struct ArtifactBy;
pub struct CollaboratorBy;
pub struct FeedCommentBy;
pub struct IssueBy;
pub struct IssueStateBy;
pub struct PullBy;
pub struct PullStatusBy;
pub struct SpindleMemberBy;

impl MirrorOf for StarBy {
    type Record = StarRecord;
    const EDGE_KIND: &'static str = "sh.tangled.feed.star.by";
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl MirrorOf for ReactionBy {
    type Record = ReactionRecord;
    const EDGE_KIND: &'static str = "sh.tangled.feed.reaction.by";
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl MirrorOf for FollowBy {
    type Record = FollowRecord;
    const EDGE_KIND: &'static str = "sh.tangled.graph.follow.by";
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl MirrorOf for VouchBy {
    type Record = VouchRecord;
    const EDGE_KIND: &'static str = "sh.tangled.graph.vouch.by";
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl MirrorOf for RefUpdateBy {
    type Record = RefUpdateRecord;
    const EDGE_KIND: &'static str = "sh.tangled.git.refUpdate.by";
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl MirrorOf for KnotMemberBy {
    type Record = KnotMemberRecord;
    const EDGE_KIND: &'static str = "sh.tangled.knot.member.by";
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl MirrorOf for LabelOpBy {
    type Record = LabelOpRecord;
    const EDGE_KIND: &'static str = "sh.tangled.label.op.by";
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl MirrorOf for PipelineBy {
    type Record = PipelineRecord;
    const EDGE_KIND: &'static str = "sh.tangled.pipeline.by";
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl MirrorOf for PipelineStatusBy {
    type Record = PipelineStatusRecord;
    const EDGE_KIND: &'static str = "sh.tangled.pipeline.status.by";
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl MirrorOf for ArtifactBy {
    type Record = ArtifactRecord;
    const EDGE_KIND: &'static str = "sh.tangled.repo.artifact.by";
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl MirrorOf for CollaboratorBy {
    type Record = CollaboratorRecord;
    const EDGE_KIND: &'static str = "sh.tangled.repo.collaborator.by";
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl MirrorOf for FeedCommentBy {
    type Record = FeedCommentRecord;
    const EDGE_KIND: &'static str = "sh.tangled.feed.comment.by";
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl MirrorOf for IssueBy {
    type Record = IssueRecord;
    const EDGE_KIND: &'static str = "sh.tangled.repo.issue.by";
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl MirrorOf for IssueStateBy {
    type Record = IssueStateRecord;
    const EDGE_KIND: &'static str = "sh.tangled.repo.issue.state.by";
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl MirrorOf for PullBy {
    type Record = PullRecord;
    const EDGE_KIND: &'static str = "sh.tangled.repo.pull.by";
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl MirrorOf for PullStatusBy {
    type Record = PullStatusRecord;
    const EDGE_KIND: &'static str = "sh.tangled.repo.pull.status.by";
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl MirrorOf for SpindleMemberBy {
    type Record = SpindleMemberRecord;
    const EDGE_KIND: &'static str = "sh.tangled.spindle.member.by";
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}

impl HasSubject for StarRecord {
    const SHAPE: SubjectShape = SubjectShape::BareDidOrOneOfCollections(&["sh.tangled.string"]);
}
impl HasSubject for FollowRecord {
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl HasSubject for IssueRecord {
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl HasSubject for PullRecord {
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl HasSubject for FeedCommentRecord {
    const SHAPE: SubjectShape = SubjectShape::OneOfCollections(&[
        "sh.tangled.repo.issue",
        "sh.tangled.repo.pull",
        "sh.tangled.string",
    ]);
}
impl HasSubject for LabelDefinitionRecord {
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl HasSubject for LabelOpRecord {
    const SHAPE: SubjectShape =
        SubjectShape::OneOfCollections(&["sh.tangled.repo.issue", "sh.tangled.repo.pull"]);
}
impl HasSubject for PipelineRecord {
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl HasSubject for PipelineStatusRecord {
    const SHAPE: SubjectShape = SubjectShape::Collection("sh.tangled.pipeline");
}
impl HasSubject for ArtifactRecord {
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl HasSubject for KnotMemberRecord {
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl HasSubject for SpindleMemberRecord {
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl HasSubject for TangledStringRecord {
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl HasSubject for ReactionRecord {
    const SHAPE: SubjectShape = SubjectShape::AnyAtUri;
}
impl HasSubject for RefUpdateRecord {
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl HasSubject for CollaboratorRecord {
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl HasSubject for IssueStateRecord {
    const SHAPE: SubjectShape = SubjectShape::Collection("sh.tangled.repo.issue");
}
impl HasSubject for PullStatusRecord {
    const SHAPE: SubjectShape = SubjectShape::Collection("sh.tangled.repo.pull");
}
impl HasSubject for KnotRecord {
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl HasSubject for SpindleRecord {
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl HasSubject for PublicKeyRecord {
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl HasSubject for RepoRecord {
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}
impl HasSubject for VouchRecord {
    const SHAPE: SubjectShape = SubjectShape::BareDid;
}

fn parse_subject(raw: &SubjectQuery, shape: SubjectShape) -> Result<SubjectRef, XrpcError> {
    let uri = match raw {
        SubjectQuery::Did(did) => {
            return match shape {
                SubjectShape::BareDid | SubjectShape::BareDidOrOneOfCollections(_) => {
                    Ok(SubjectRef::Did(did.clone()))
                }
                SubjectShape::Collection(expected) => Err(XrpcError::InvalidParams(format!(
                    "subject must be at://<did>/{expected}/<rkey>, got bare did"
                ))),
                SubjectShape::OneOfCollections(allowed) => Err(XrpcError::InvalidParams(format!(
                    "subject must be at://<did>/<nsid>/<rkey> with nsid in [{}], got bare did",
                    allowed.join(", "),
                ))),
                SubjectShape::AnyAtUri => Err(XrpcError::InvalidParams(
                    "subject must be at-uri form, got bare did".into(),
                )),
            };
        }
        SubjectQuery::Uri(uri) => uri,
    };
    if matches!(uri.authority(), AtIdentifier::Handle(_)) {
        return Err(XrpcError::InvalidParams(
            "subject authority must be a did, not a handle".into(),
        ));
    }
    let Some(collection) = uri.collection() else {
        return Err(XrpcError::InvalidParams(
            "subject must be a bare did or full at://<did>/<nsid>/<rkey>".into(),
        ));
    };
    let c = collection.as_ref();
    match shape {
        SubjectShape::BareDid => Err(XrpcError::InvalidParams(format!(
            "subject must be a bare did, got at-uri with collection {c}"
        ))),
        SubjectShape::Collection(expected) if c == expected => {
            require_rkey(uri, expected)?;
            Ok(SubjectRef::Uri(uri.clone()))
        }
        SubjectShape::Collection(expected) => Err(XrpcError::InvalidParams(format!(
            "subject must be at://<did>/{expected}/<rkey>, got collection {c}"
        ))),
        SubjectShape::OneOfCollections(allowed) if allowed.contains(&c) => {
            require_rkey(uri, c)?;
            Ok(SubjectRef::Uri(uri.clone()))
        }
        SubjectShape::OneOfCollections(allowed) => Err(XrpcError::InvalidParams(format!(
            "subject must be at://<did>/<nsid>/<rkey> with nsid in [{}], got collection {c}",
            allowed.join(", "),
        ))),
        SubjectShape::BareDidOrOneOfCollections(allowed) if allowed.contains(&c) => {
            require_rkey(uri, c)?;
            Ok(SubjectRef::Uri(uri.clone()))
        }
        SubjectShape::BareDidOrOneOfCollections(allowed) => Err(XrpcError::InvalidParams(format!(
            "subject must be a bare did or at://<did>/<nsid>/<rkey> with nsid in [{}], got collection {c}",
            allowed.join(", "),
        ))),
        SubjectShape::AnyAtUri => Ok(SubjectRef::Uri(uri.clone())),
    }
}

fn require_rkey(uri: &AtUri<DefaultStr>, expected: &str) -> Result<(), XrpcError> {
    uri.rkey().map(|_| ()).ok_or_else(|| {
        XrpcError::InvalidParams(format!(
            "subject must be at://<did>/{expected}/<rkey>; missing rkey"
        ))
    })
}

fn parse_cursor(raw: Option<&str>) -> Result<PageCursor, XrpcError> {
    PageCursor::from_token(raw)
        .map_err(|e: CursorParseError| XrpcError::InvalidParams(format!("cursor: {e}")))
}

fn parse_limit(raw: Option<u32>) -> Result<PageLimit, XrpcError> {
    PageLimit::new(raw.unwrap_or(DEFAULT_LIMIT))
        .map_err(|e| XrpcError::InvalidParams(format!("limit: {e}")))
}

pub(crate) fn at_uri_owned_by(uri: &AtUri<DefaultStr>, author: &Did<DefaultStr>) -> bool {
    match uri.authority() {
        AtIdentifier::Did(d) => d.as_ref() == author.as_ref(),
        AtIdentifier::Handle(_) => false,
    }
}

async fn resolve_for_view(
    state: &AppState,
    expected_nsid: &Nsid<DefaultStr>,
    uri: AtUri<DefaultStr>,
) -> Result<Arc<RecordBody>, XrpcError> {
    let raw = uri.as_ref().to_owned();
    resolve(state, ExpectedNsid::new(expected_nsid.clone()), uri)
        .await
        .map(|(body, _did)| body)
        .map_err(|e| match e {
            XrpcError::NotFound => XrpcError::UpstreamGone(raw),
            other => other,
        })
}

async fn resolve(
    state: &AppState,
    expected: ExpectedNsid,
    uri: AtUri<DefaultStr>,
) -> Result<(Arc<RecordBody>, Did<DefaultStr>), XrpcError> {
    let collection = uri
        .collection()
        .ok_or_else(|| XrpcError::InvalidParams("uri missing collection".into()))?;
    if !expected.accepts(collection.as_ref()) {
        return Err(XrpcError::InvalidParams(format!(
            "collection mismatch: expected {}, got {}",
            expected.as_str(),
            collection.as_ref()
        )));
    }
    let rkey = uri
        .rkey()
        .ok_or_else(|| XrpcError::InvalidParams("uri missing rkey".into()))?;
    let did_ref = match uri.authority() {
        AtIdentifier::Did(d) => d,
        AtIdentifier::Handle(_) => {
            return Err(XrpcError::InvalidParams(
                "uri authority must be a did, not a handle".into(),
            ));
        }
    };
    let did: Did<DefaultStr> = did_ref.clone().into_static();

    if let Some(hit) = state.records.get(&uri) {
        return Ok((hit, did));
    }
    let body = state
        .slingshot
        .get_record(&did_ref, &collection, &rkey)
        .await
        .map_err(map_slingshot)?;
    verify_type_tag(&body, &expected)?;
    state.records.put(uri, body.clone());
    Ok((body, did))
}

#[derive(Deserialize)]
struct TypeTag<'a> {
    #[serde(rename = "$type", borrow)]
    ty: &'a str,
}

fn verify_type_tag(body: &RecordBody, expected: &ExpectedNsid) -> Result<(), XrpcError> {
    let bytes = body.value.as_ref();
    let ty: std::borrow::Cow<'_, str> = match serde_json::from_slice::<TypeTag>(bytes) {
        Ok(t) => std::borrow::Cow::Borrowed(t.ty),
        Err(_) => {
            let value: serde_json::Value = serde_json::from_slice(bytes)
                .map_err(|e| XrpcError::InvalidRecord(format!("$type peek: {e}")))?;
            value
                .as_object()
                .and_then(|m| m.get("$type"))
                .and_then(|v| v.as_str())
                .map(|s| std::borrow::Cow::Owned(s.to_owned()))
                .ok_or_else(|| XrpcError::InvalidRecord("$type peek: missing $type field".into()))?
        }
    };
    if !expected.accepts(ty.as_ref()) {
        return Err(XrpcError::InvalidRecord(format!(
            "$type mismatch: expected {}, got {}",
            expected.as_str(),
            ty
        )));
    }
    Ok(())
}

fn wire_type_nsid(bytes: &[u8]) -> Option<Nsid<DefaultStr>> {
    let ty = serde_json::from_slice::<TypeTag>(bytes).ok()?.ty;
    Nsid::<DefaultStr>::new_owned(ty).ok()
}

async fn deserialize_or_upgrade<V>(
    state: &AppState,
    nsid: &Nsid<DefaultStr>,
    bytes: &[u8],
) -> Result<V, XrpcError>
where
    V: serde::de::DeserializeOwned,
{
    match serde_json::from_slice::<V>(bytes) {
        Ok(v) => Ok(v),
        Err(canon_err) => {
            let normalized = normalize_record_fields(bytes);
            let working: &[u8] = normalized.as_deref().unwrap_or(bytes);
            if normalized.is_some()
                && let Ok(v) = serde_json::from_slice::<V>(working)
            {
                return Ok(v);
            }
            let scrubbed = scrub_record_bytes(nsid, working);
            let retry_bytes: &[u8] = scrubbed.as_deref().unwrap_or(working);
            if scrubbed.is_some()
                && let Ok(v) = serde_json::from_slice::<V>(retry_bytes)
            {
                return Ok(v);
            }
            let wire_nsid = wire_type_nsid(retry_bytes).unwrap_or_else(|| nsid.clone());
            match upgrade_wire_bytes(&wire_nsid, retry_bytes, &state.resolver).await {
                Ok(canon_bytes) => serde_json::from_slice(&canon_bytes)
                    .map_err(|e| XrpcError::InvalidRecord(e.to_string())),
                Err(_) => Err(XrpcError::InvalidRecord(canon_err.to_string())),
            }
        }
    }
}

async fn fetch_from_uri<R, V>(
    state: &AppState,
    uri: AtUri<DefaultStr>,
) -> Result<(Arc<RecordBody>, V), XrpcError>
where
    R: XrpcResp,
    V: serde::de::DeserializeOwned + NormalizeRepoRefs,
{
    let raw = uri.as_str().to_owned();
    let nsid = nsid_static(R::NSID);
    let (body, _did) = resolve(state, ExpectedNsid::new(nsid.clone()), uri).await?;
    let value: V = deserialize_or_upgrade(state, &nsid, &body.value).await?;
    let value = value
        .normalize(&state.resolver)
        .await
        .ok_or(XrpcError::UpstreamGone(raw))?;
    Ok((body, value))
}

async fn fetch<R, V>(
    state: &AppState,
    uri: &AtUri<DefaultStr>,
) -> Result<(Arc<RecordBody>, V), XrpcError>
where
    R: XrpcResp,
    V: serde::de::DeserializeOwned + NormalizeRepoRefs,
{
    fetch_from_uri::<R, V>(state, uri.clone()).await
}

async fn get_repo(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<GetRepoQuery>,
) -> Result<Json<Deduped<RepoGetRecordOutput<DefaultStr>>>, XrpcError> {
    let (body, value) = fetch::<RepoRecord, Repo<DefaultStr>>(&state, &q.repo).await?;
    Ok(Json(Deduped(RepoGetRecordOutput {
        cid: Some(body.cid.clone()),
        uri: body.uri.clone(),
        value,
    })))
}

async fn get_repo_by_repo_did(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<GetRepoByRepoDidQuery>,
) -> Result<Json<Deduped<RepoGetRecordOutput<DefaultStr>>>, XrpcError> {
    let ident = state
        .resolver
        .lookup_by_repo_did(&q.repo_did)
        .await
        .ok_or(XrpcError::NotFound)?;
    let uri = AtUri::<DefaultStr>::from_parts_owned(
        ident.owner.as_str(),
        RepoRecord::NSID,
        ident.rkey.as_str(),
    )
    .expect("Did and Rkey newtypes already validated, at-uri assembly cannot fail");
    let (body, value) = fetch_from_uri::<RepoRecord, Repo<DefaultStr>>(&state, uri).await?;
    Ok(Json(Deduped(RepoGetRecordOutput {
        cid: Some(body.cid.clone()),
        uri: body.uri.clone(),
        value,
    })))
}

async fn get_profile(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<GetProfileQuery>,
) -> Result<Json<Deduped<ProfileGetRecordOutput<DefaultStr>>>, XrpcError> {
    let (body, value) = fetch::<ProfileRecord, Profile<DefaultStr>>(&state, &q.actor).await?;
    Ok(Json(Deduped(ProfileGetRecordOutput {
        cid: Some(body.cid.clone()),
        uri: body.uri.clone(),
        value,
    })))
}

async fn get_issue(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<GetIssueQuery>,
) -> Result<Json<Deduped<IssueGetRecordOutput<DefaultStr>>>, XrpcError> {
    let (body, value) = fetch::<IssueRecord, Issue<DefaultStr>>(&state, &q.issue).await?;
    Ok(Json(Deduped(IssueGetRecordOutput {
        cid: Some(body.cid.clone()),
        uri: body.uri.clone(),
        value,
    })))
}

async fn get_pull(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<GetPullQuery>,
) -> Result<Json<Deduped<PullGetRecordOutput<DefaultStr>>>, XrpcError> {
    let (body, value) = fetch::<PullRecord, Pull<DefaultStr>>(&state, &q.pull).await?;
    Ok(Json(Deduped(PullGetRecordOutput {
        cid: Some(body.cid.clone()),
        uri: body.uri.clone(),
        value,
    })))
}

async fn get_repos(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
) -> Result<Response, XrpcError> {
    let uris = collect_repeated(query.as_deref(), BULK_REPOS_KEY);
    bulk_fetch::<RepoRecord, Repo<DefaultStr>>(&state, uris).await
}

async fn get_profiles(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
) -> Result<Response, XrpcError> {
    let uris = collect_repeated(query.as_deref(), BULK_PROFILES_KEY);
    bulk_fetch::<ProfileRecord, Profile<DefaultStr>>(&state, uris).await
}

async fn get_issues(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
) -> Result<Response, XrpcError> {
    let uris = collect_repeated(query.as_deref(), BULK_ISSUES_KEY);
    bulk_fetch::<IssueRecord, Issue<DefaultStr>>(&state, uris).await
}

async fn get_pulls(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
) -> Result<Response, XrpcError> {
    let uris = collect_repeated(query.as_deref(), BULK_PULLS_KEY);
    bulk_fetch::<PullRecord, Pull<DefaultStr>>(&state, uris).await
}

const BULK_REPOS_KEY: &str = "repos";
const BULK_PROFILES_KEY: &str = "actors";
const BULK_ISSUES_KEY: &str = "issues";
const BULK_PULLS_KEY: &str = "pulls";
const BULK_LIMIT: usize = 50;

fn collect_repeated(query: Option<&str>, key: &str) -> Vec<String> {
    let Some(q) = query else {
        return Vec::new();
    };
    form_urlencoded::parse(q.as_bytes())
        .filter_map(|(k, v)| (k == key).then(|| v.into_owned()))
        .collect()
}

async fn hydrate_record_view<V>(
    state: &AppState,
    nsid: &Nsid<DefaultStr>,
    uri: AtUri<DefaultStr>,
    sort_micros: u64,
) -> Result<Option<RecordView<V>>, XrpcError>
where
    V: serde::de::DeserializeOwned + NormalizeRepoRefs,
{
    if let Some(source) = decode_knot_owned_source(&uri) {
        return synthesize_knot_owned_view::<V>(state, uri, source, sort_micros).await;
    }
    let body = resolve_for_view(state, nsid, uri).await?;
    let value: V = deserialize_or_upgrade::<V>(state, nsid, &body.value).await?;
    let Some(value) = value.normalize(&state.resolver).await else {
        return Ok(None);
    };
    Ok(Some(RecordView {
        uri: body.uri.clone(),
        cid: Some(body.cid.clone()),
        value,
    }))
}

async fn synthesize_knot_owned_view<V>(
    state: &AppState,
    uri: AtUri<DefaultStr>,
    source: KnotOwnedSource,
    sort_micros: u64,
) -> Result<Option<RecordView<V>>, XrpcError>
where
    V: serde::de::DeserializeOwned + NormalizeRepoRefs,
{
    let Some(body) = synth_knot_owned_value(source, sort_micros) else {
        return Ok(None);
    };
    let Ok(value) = serde_json::from_value::<V>(body) else {
        return Ok(None);
    };
    let Some(value) = value.normalize(&state.resolver).await else {
        return Ok(None);
    };
    Ok(Some(RecordView {
        uri,
        cid: None,
        value,
    }))
}

fn synth_knot_owned_value(source: KnotOwnedSource, sort_micros: u64) -> Option<serde_json::Value> {
    let created_at = micros_to_rfc3339(sort_micros)?;
    match source {
        KnotOwnedSource::Member { knot, subject } => Some(serde_json::json!({
            "domain": knot_did_host(&knot)?,
            "subject": subject.as_ref(),
            "createdAt": created_at,
        })),
        KnotOwnedSource::Collaborator { repo, subject } => Some(serde_json::json!({
            "repo": repo.as_ref(),
            "subject": subject.as_ref(),
            "createdAt": created_at,
        })),
    }
}

fn micros_to_rfc3339(micros: u64) -> Option<String> {
    let micros = i64::try_from(micros).ok()?;
    chrono::DateTime::from_timestamp_micros(micros)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
}

#[derive(Clone, Copy)]
enum HitProvenance {
    ClientSupplied,
    Indexed,
}

fn is_index_evictable(err: &XrpcError) -> bool {
    matches!(
        err,
        XrpcError::NotFound
            | XrpcError::UpstreamGone(_)
            | XrpcError::InvalidRecord(_)
            | XrpcError::InvalidParams(_)
    )
}

fn drop_unhydratable<V>(
    provenance: HitProvenance,
    nsid: &Nsid<DefaultStr>,
    uri: &AtUri<DefaultStr>,
    result: Result<Option<V>, XrpcError>,
) -> Result<Option<V>, XrpcError> {
    match result {
        Ok(view) => Ok(view),
        Err(err @ (XrpcError::NotFound | XrpcError::UpstreamGone(_))) => {
            tracing::debug!(
                uri = %uri,
                nsid = %nsid.as_ref(),
                error = %err,
                "dropping gone hit during hydration",
            );
            Ok(None)
        }
        Err(err @ XrpcError::UpstreamUnavailable(_)) => {
            tracing::warn!(
                uri = %uri,
                nsid = %nsid.as_ref(),
                error = %err,
                "dropping hit, upstream unavailable during hydration",
            );
            Ok(None)
        }
        Err(err @ XrpcError::InvalidRecord(_)) => {
            tracing::warn!(
                uri = %uri,
                nsid = %nsid.as_ref(),
                error = %err,
                "dropping invalid hit during hydration",
            );
            Ok(None)
        }
        Err(err @ XrpcError::InvalidParams(_)) => match provenance {
            HitProvenance::ClientSupplied => Err(err),
            HitProvenance::Indexed => {
                tracing::warn!(
                    uri = %uri,
                    nsid = %nsid.as_ref(),
                    error = %err,
                    "dropping malformed indexed hit during hydration",
                );
                Ok(None)
            }
        },
        Err(err @ (XrpcError::Internal(_) | XrpcError::Overloaded)) => Err(err),
    }
}

fn hydrate_stream<T, Fut, V>(
    items: impl IntoIterator<Item = T>,
    produce: impl FnMut(T) -> Fut,
) -> impl Stream<Item = Result<V, XrpcError>>
where
    Fut: Future<Output = Result<Option<V>, XrpcError>>,
{
    stream::iter(items)
        .map(produce)
        .buffered(FETCH_CONCURRENCY)
        .try_filter_map(|view| async move { Ok(view) })
}

fn hydrate_record_stream<V>(
    state: &AppState,
    nsid: Nsid<DefaultStr>,
    items: Vec<EdgeItem>,
    provenance: HitProvenance,
) -> impl Stream<Item = Result<RecordView<V>, XrpcError>> + Send + 'static
where
    V: serde::de::DeserializeOwned + Serialize + NormalizeRepoRefs + Send + 'static,
{
    let owned = state.clone();
    hydrate_stream(items, move |item| {
        let owned = owned.clone();
        let nsid = nsid.clone();
        async move {
            let EdgeItem { uri, sort_micros } = item;
            let result = hydrate_record_view::<V>(&owned, &nsid, uri.clone(), sort_micros).await;
            if matches!(provenance, HitProvenance::Indexed)
                && let Err(err) = &result
                && is_index_evictable(err)
            {
                owned.edges.remove_source(&uri);
            }
            drop_unhydratable(provenance, &nsid, &uri, result)
        }
    })
}

enum PagePhase {
    Head,
    Body { first: bool },
    Done,
}

struct PageState<S> {
    items: std::pin::Pin<Box<S>>,
    phase: PagePhase,
    array_key: &'static str,
    tail: Vec<u8>,
    permit: Option<HeavyPermit>,
}

fn paged_tail(cursor: Option<String>) -> Vec<u8> {
    let encoded = serde_json::to_string(&cursor).unwrap_or_else(|_| "null".to_owned());
    format!("],\"cursor\":{encoded}}}").into_bytes()
}

fn unpaged_tail() -> Vec<u8> {
    b"]}".to_vec()
}

fn json_stream<V, S>(
    array_key: &'static str,
    items: S,
    tail: Vec<u8>,
    permit: Option<HeavyPermit>,
) -> Response
where
    V: Serialize + Send + 'static,
    S: Stream<Item = Result<V, XrpcError>> + Send + 'static,
{
    let init = PageState {
        items: Box::pin(items),
        phase: PagePhase::Head,
        array_key,
        tail,
        permit,
    };
    let chunks = stream::unfold(init, |mut st| async move {
        match st.phase {
            PagePhase::Head => {
                let head = format!("{{\"{}\":[", st.array_key).into_bytes();
                st.phase = PagePhase::Body { first: true };
                Some((Ok::<Vec<u8>, Infallible>(head), st))
            }
            PagePhase::Body { first } => match st.items.next().await {
                Some(Ok(view)) => match serde_json::to_vec(&Deduped(&view)) {
                    Ok(encoded) => {
                        let mut chunk = Vec::with_capacity(encoded.len() + 1);
                        if !first {
                            chunk.push(b',');
                        }
                        chunk.extend_from_slice(&encoded);
                        st.phase = PagePhase::Body { first: false };
                        Some((Ok(chunk), st))
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "skipping hit, serialize failed mid-stream");
                        Some((Ok(Vec::new()), st))
                    }
                },
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "ending page early, hydration failed mid-stream");
                    let tail = std::mem::take(&mut st.tail);
                    st.phase = PagePhase::Done;
                    Some((Ok(tail), st))
                }
                None => {
                    let tail = std::mem::take(&mut st.tail);
                    st.phase = PagePhase::Done;
                    Some((Ok(tail), st))
                }
            },
            PagePhase::Done => {
                drop(st.permit.take());
                None
            }
        }
    });
    (
        [(CONTENT_TYPE, "application/json")],
        Body::from_stream(chunks),
    )
        .into_response()
}

async fn bulk_fetch<R, V>(state: &AppState, uris: Vec<String>) -> Result<Response, XrpcError>
where
    R: XrpcResp,
    V: serde::de::DeserializeOwned + Serialize + NormalizeRepoRefs + Send + 'static,
{
    if uris.is_empty() {
        return Err(XrpcError::InvalidParams("at least one uri required".into()));
    }
    if uris.len() > BULK_LIMIT {
        return Err(XrpcError::InvalidParams(format!(
            "at most {BULK_LIMIT} uris per request"
        )));
    }
    let parsed: Vec<AtUri<DefaultStr>> = uris
        .iter()
        .map(|s| parse_uri(s))
        .collect::<Result<_, _>>()?;
    let nsid = nsid_static(R::NSID);
    if let Some(bad) = parsed
        .iter()
        .find(|uri| uri.collection().is_none_or(|c| c.as_ref() != nsid.as_ref()))
    {
        return Err(XrpcError::InvalidParams(format!(
            "uri collection must be {}, got {}",
            nsid.as_ref(),
            bad.as_ref()
        )));
    }
    let permit = state.heavy_permit()?;
    let items = parsed
        .into_iter()
        .map(|uri| EdgeItem {
            uri,
            sort_micros: 0,
        })
        .collect();
    let views = hydrate_record_stream::<V>(state, nsid, items, HitProvenance::ClientSupplied);
    Ok(json_stream::<RecordView<V>, _>(
        "items",
        views,
        unpaged_tail(),
        permit,
    ))
}

fn record_edge_page<R, F>(
    state: &AppState,
    q: &TypedListQuery<F>,
) -> Result<(EdgePage, Nsid<DefaultStr>), XrpcError>
where
    R: XrpcResp + HasSubject,
    F: ListFilter,
{
    let subject = parse_subject(&q.subject, R::SHAPE)?;
    let cursor = parse_cursor(q.cursor.as_deref())?;
    let limit = parse_limit(q.limit)?;
    let dir = q.dir();
    let nsid = nsid_static(R::NSID);
    let page = if q.filter.is_identity() {
        let key = EdgeKey::new(nsid.clone(), subject);
        state.edges.list(&key, cursor, limit, dir)
    } else {
        let pred = q.filter.predicate(state, &subject);
        let key = EdgeKey::new(nsid.clone(), subject);
        state.edges.list_filtered(&key, cursor, limit, dir, pred)
    };
    Ok((page, nsid))
}

async fn list_records<R, V, F>(
    state: &AppState,
    q: TypedListQuery<F>,
) -> Result<Response, XrpcError>
where
    R: XrpcResp + HasSubject,
    V: serde::de::DeserializeOwned + Serialize + NormalizeRepoRefs + Send + 'static,
    F: ListFilter,
{
    let (page, nsid) = record_edge_page::<R, F>(state, &q)?;
    let permit = state.heavy_permit()?;
    let views = hydrate_record_stream::<V>(state, nsid, page.items, HitProvenance::Indexed);
    Ok(json_stream::<RecordView<V>, _>(
        "items",
        views,
        paged_tail(page.next.map(PageToken::encode_token)),
        permit,
    ))
}

fn count_for<R: XrpcResp + HasSubject>(
    state: &AppState,
    q: CountQuery,
) -> Result<CountResponse, XrpcError> {
    let subject = parse_subject(&q.subject, R::SHAPE)?;
    let key = EdgeKey::new(nsid_static(R::NSID), subject);
    Ok(CountResponse {
        count: state.edges.count(&key),
        distinct_authors: state.edges.count_distinct_authors(&key),
    })
}

fn mirror_edge_page<M, F>(
    state: &AppState,
    q: &TypedListQuery<F>,
) -> Result<(EdgePage, Nsid<DefaultStr>), XrpcError>
where
    M: MirrorOf,
    F: ListFilter,
{
    let subject = parse_subject(&q.subject, M::SHAPE)?;
    let cursor = parse_cursor(q.cursor.as_deref())?;
    let limit = parse_limit(q.limit)?;
    let dir = q.dir();
    let edge_nsid = nsid_static(M::EDGE_KIND);
    let page = if q.filter.is_identity() {
        let key = EdgeKey::new(edge_nsid, subject);
        state.edges.list(&key, cursor, limit, dir)
    } else {
        let pred = q.filter.predicate(state, &subject);
        let key = EdgeKey::new(edge_nsid, subject);
        state.edges.list_filtered(&key, cursor, limit, dir, pred)
    };
    let record_nsid = nsid_static(<M::Record as XrpcResp>::NSID);
    Ok((page, record_nsid))
}

async fn list_mirror<M, V, F>(state: &AppState, q: TypedListQuery<F>) -> Result<Response, XrpcError>
where
    M: MirrorOf,
    V: serde::de::DeserializeOwned + Serialize + NormalizeRepoRefs + Send + 'static,
    F: ListFilter,
{
    let (page, record_nsid) = mirror_edge_page::<M, F>(state, &q)?;
    let permit = state.heavy_permit()?;
    let views = hydrate_record_stream::<V>(state, record_nsid, page.items, HitProvenance::Indexed);
    Ok(json_stream::<RecordView<V>, _>(
        "items",
        views,
        paged_tail(page.next.map(PageToken::encode_token)),
        permit,
    ))
}

fn count_mirror<M: MirrorOf>(state: &AppState, q: CountQuery) -> Result<CountResponse, XrpcError> {
    let subject = parse_subject(&q.subject, M::SHAPE)?;
    let key = EdgeKey::new(nsid_static(M::EDGE_KIND), subject);
    Ok(CountResponse {
        count: state.edges.count(&key),
        distinct_authors: state.edges.count_distinct_authors(&key),
    })
}

async fn list_stars(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_records::<StarRecord, Star<DefaultStr>, _>(&state, q).await
}

async fn count_stars(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_for::<StarRecord>(&state, q).map(Json)
}

async fn list_follows(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_records::<FollowRecord, Follow<DefaultStr>, _>(&state, q).await
}

async fn count_follows(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_for::<FollowRecord>(&state, q).map(Json)
}

async fn list_issues(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<IssueFilter>>,
) -> Result<Response, XrpcError> {
    let (page, nsid) = record_edge_page::<IssueRecord, _>(&state, &q)?;
    let permit = state.heavy_permit()?;
    let owned = state.clone();
    let items = hydrate_record_stream::<Issue<DefaultStr>>(
        &state,
        nsid,
        page.items,
        HitProvenance::Indexed,
    )
    .map(move |view| view.map(|v| enrich_issue_view(&owned, v)));
    Ok(json_stream::<StatefulItem<Issue<DefaultStr>>, _>(
        "items",
        items,
        paged_tail(page.next.map(PageToken::encode_token)),
        permit,
    ))
}

async fn count_issues(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_for::<IssueRecord>(&state, q).map(Json)
}

async fn list_pulls(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<PullFilter>>,
) -> Result<Response, XrpcError> {
    let (page, nsid) = record_edge_page::<PullRecord, _>(&state, &q)?;
    let permit = state.heavy_permit()?;
    let owned = state.clone();
    let items =
        hydrate_record_stream::<Pull<DefaultStr>>(&state, nsid, page.items, HitProvenance::Indexed)
            .map(move |view| view.map(|v| enrich_pull_view(&owned, v)));
    Ok(json_stream::<StatefulItem<Pull<DefaultStr>>, _>(
        "items",
        items,
        paged_tail(page.next.map(PageToken::encode_token)),
        permit,
    ))
}

async fn count_pulls(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_for::<PullRecord>(&state, q).map(Json)
}

async fn list_feed_comments(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_records::<FeedCommentRecord, FeedComment<DefaultStr>, _>(&state, q).await
}

async fn count_feed_comments(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_for::<FeedCommentRecord>(&state, q).map(Json)
}

async fn list_reactions(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_records::<ReactionRecord, Reaction<DefaultStr>, _>(&state, q).await
}

async fn count_reactions(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_for::<ReactionRecord>(&state, q).map(Json)
}

async fn list_ref_updates(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_records::<RefUpdateRecord, RefUpdate<DefaultStr>, _>(&state, q).await
}

async fn count_ref_updates(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_for::<RefUpdateRecord>(&state, q).map(Json)
}

async fn list_collaborators(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_records::<CollaboratorRecord, Collaborator<DefaultStr>, _>(&state, q).await
}

async fn count_collaborators(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_for::<CollaboratorRecord>(&state, q).map(Json)
}

async fn list_issue_states(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_records::<IssueStateRecord, IssueState<DefaultStr>, _>(&state, q).await
}

async fn count_issue_states(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_for::<IssueStateRecord>(&state, q).map(Json)
}

async fn list_pull_statuses(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_records::<PullStatusRecord, PullStatus<DefaultStr>, _>(&state, q).await
}

async fn count_pull_statuses(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_for::<PullStatusRecord>(&state, q).map(Json)
}

async fn list_repos(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_records::<RepoRecord, Repo<DefaultStr>, _>(&state, q).await
}

async fn count_repos(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_for::<RepoRecord>(&state, q).map(Json)
}

async fn list_knots(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_records::<KnotRecord, Knot<DefaultStr>, _>(&state, q).await
}

async fn count_knots(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_for::<KnotRecord>(&state, q).map(Json)
}

async fn list_spindles(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_records::<SpindleRecord, Spindle<DefaultStr>, _>(&state, q).await
}

async fn count_spindles(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_for::<SpindleRecord>(&state, q).map(Json)
}

async fn list_public_keys(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_records::<PublicKeyRecord, PublicKey<DefaultStr>, _>(&state, q).await
}

async fn count_public_keys(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_for::<PublicKeyRecord>(&state, q).map(Json)
}

async fn list_vouches(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_records::<VouchRecord, Vouch<DefaultStr>, _>(&state, q).await
}

async fn count_vouches(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_for::<VouchRecord>(&state, q).map(Json)
}

async fn list_stars_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_mirror::<StarBy, Star<DefaultStr>, _>(&state, q).await
}
async fn count_stars_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_mirror::<StarBy>(&state, q).map(Json)
}

async fn list_reactions_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_mirror::<ReactionBy, Reaction<DefaultStr>, _>(&state, q).await
}
async fn count_reactions_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_mirror::<ReactionBy>(&state, q).map(Json)
}

async fn list_follows_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_mirror::<FollowBy, Follow<DefaultStr>, _>(&state, q).await
}
async fn count_follows_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_mirror::<FollowBy>(&state, q).map(Json)
}

async fn list_vouches_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_mirror::<VouchBy, Vouch<DefaultStr>, _>(&state, q).await
}
async fn count_vouches_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_mirror::<VouchBy>(&state, q).map(Json)
}

async fn list_ref_updates_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_mirror::<RefUpdateBy, RefUpdate<DefaultStr>, _>(&state, q).await
}
async fn count_ref_updates_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_mirror::<RefUpdateBy>(&state, q).map(Json)
}

async fn list_knot_members_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_mirror::<KnotMemberBy, KnotMember<DefaultStr>, _>(&state, q).await
}
async fn count_knot_members_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_mirror::<KnotMemberBy>(&state, q).map(Json)
}

async fn list_label_ops_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_mirror::<LabelOpBy, LabelOp<DefaultStr>, _>(&state, q).await
}
async fn count_label_ops_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_mirror::<LabelOpBy>(&state, q).map(Json)
}

async fn list_pipelines_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_mirror::<PipelineBy, Pipeline<DefaultStr>, _>(&state, q).await
}
async fn count_pipelines_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_mirror::<PipelineBy>(&state, q).map(Json)
}

async fn list_pipeline_statuses_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_mirror::<PipelineStatusBy, PipelineStatus<DefaultStr>, _>(&state, q).await
}
async fn count_pipeline_statuses_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_mirror::<PipelineStatusBy>(&state, q).map(Json)
}

async fn list_artifacts_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_mirror::<ArtifactBy, Artifact<DefaultStr>, _>(&state, q).await
}
async fn count_artifacts_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_mirror::<ArtifactBy>(&state, q).map(Json)
}

async fn list_collaborators_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_mirror::<CollaboratorBy, Collaborator<DefaultStr>, _>(&state, q).await
}
async fn count_collaborators_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_mirror::<CollaboratorBy>(&state, q).map(Json)
}

async fn list_issues_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<IssueFilter>>,
) -> Result<Response, XrpcError> {
    let (page, nsid) = mirror_edge_page::<IssueBy, _>(&state, &q)?;
    let permit = state.heavy_permit()?;
    let owned = state.clone();
    let items = hydrate_record_stream::<Issue<DefaultStr>>(
        &state,
        nsid,
        page.items,
        HitProvenance::Indexed,
    )
    .map(move |view| view.map(|v| enrich_issue_view(&owned, v)));
    Ok(json_stream::<StatefulItem<Issue<DefaultStr>>, _>(
        "items",
        items,
        paged_tail(page.next.map(PageToken::encode_token)),
        permit,
    ))
}
async fn count_issues_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_mirror::<IssueBy>(&state, q).map(Json)
}

async fn list_feed_comments_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_mirror::<FeedCommentBy, FeedComment<DefaultStr>, _>(&state, q).await
}
async fn count_feed_comments_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_mirror::<FeedCommentBy>(&state, q).map(Json)
}

async fn list_issue_states_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_mirror::<IssueStateBy, IssueState<DefaultStr>, _>(&state, q).await
}
async fn count_issue_states_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_mirror::<IssueStateBy>(&state, q).map(Json)
}

async fn list_pulls_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<PullFilter>>,
) -> Result<Response, XrpcError> {
    let (page, nsid) = mirror_edge_page::<PullBy, _>(&state, &q)?;
    let permit = state.heavy_permit()?;
    let owned = state.clone();
    let items =
        hydrate_record_stream::<Pull<DefaultStr>>(&state, nsid, page.items, HitProvenance::Indexed)
            .map(move |view| view.map(|v| enrich_pull_view(&owned, v)));
    Ok(json_stream::<StatefulItem<Pull<DefaultStr>>, _>(
        "items",
        items,
        paged_tail(page.next.map(PageToken::encode_token)),
        permit,
    ))
}
async fn count_pulls_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_mirror::<PullBy>(&state, q).map(Json)
}

async fn list_pull_statuses_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_mirror::<PullStatusBy, PullStatus<DefaultStr>, _>(&state, q).await
}
async fn count_pull_statuses_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_mirror::<PullStatusBy>(&state, q).map(Json)
}

async fn list_spindle_members_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_mirror::<SpindleMemberBy, SpindleMember<DefaultStr>, _>(&state, q).await
}
async fn count_spindle_members_by(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_mirror::<SpindleMemberBy>(&state, q).map(Json)
}

async fn list_label_definitions(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_records::<LabelDefinitionRecord, LabelDefinition<DefaultStr>, _>(&state, q).await
}

async fn count_label_definitions(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_for::<LabelDefinitionRecord>(&state, q).map(Json)
}

async fn list_label_ops(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_records::<LabelOpRecord, LabelOp<DefaultStr>, _>(&state, q).await
}

async fn count_label_ops(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_for::<LabelOpRecord>(&state, q).map(Json)
}

async fn list_pipelines(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_records::<PipelineRecord, Pipeline<DefaultStr>, _>(&state, q).await
}

async fn count_pipelines(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_for::<PipelineRecord>(&state, q).map(Json)
}

async fn list_pipeline_statuses(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_records::<PipelineStatusRecord, PipelineStatus<DefaultStr>, _>(&state, q).await
}

async fn count_pipeline_statuses(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_for::<PipelineStatusRecord>(&state, q).map(Json)
}

async fn list_artifacts(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_records::<ArtifactRecord, Artifact<DefaultStr>, _>(&state, q).await
}

async fn count_artifacts(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_for::<ArtifactRecord>(&state, q).map(Json)
}

async fn list_knot_members(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_records::<KnotMemberRecord, KnotMember<DefaultStr>, _>(&state, q).await
}

async fn count_knot_members(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_for::<KnotMemberRecord>(&state, q).map(Json)
}

async fn list_spindle_members(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_records::<SpindleMemberRecord, SpindleMember<DefaultStr>, _>(&state, q).await
}

async fn count_spindle_members(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_for::<SpindleMemberRecord>(&state, q).map(Json)
}

async fn list_strings(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<TypedListQuery<NoFilter>>,
) -> Result<Response, XrpcError> {
    list_records::<TangledStringRecord, TangledString<DefaultStr>, _>(&state, q).await
}

async fn count_strings(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<CountQuery>,
) -> Result<Json<CountResponse>, XrpcError> {
    count_for::<TangledStringRecord>(&state, q).map(Json)
}

#[derive(Deserialize)]
struct ResolveMiniDocParams {
    identifier: AtIdentifier<DefaultStr>,
}

async fn resolve_mini_doc(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<ResolveMiniDocParams>,
) -> Result<Response, XrpcError> {
    let body = state
        .slingshot
        .resolve_mini_doc(&q.identifier)
        .await
        .map_err(map_slingshot)?;
    Ok((StatusCode::OK, [(CONTENT_TYPE, "application/json")], body).into_response())
}

async fn get_coverage(State(state): State<AppState>) -> Json<CoverageEnvelope> {
    Json(state.coverage.snapshot().into())
}

async fn search_query(
    State(state): State<AppState>,
    XrpcQuery(q): XrpcQuery<SearchQueryParams>,
) -> Result<Response, XrpcError> {
    if q.q.trim().is_empty() {
        return Err(XrpcError::InvalidParams("q must not be empty".into()));
    }
    let cursor = SearchCursor::from_token(q.cursor.as_deref())
        .map_err(|e| XrpcError::InvalidParams(format!("cursor: {e}")))?;
    let limit = parse_limit(q.limit)?;
    let filters = build_search_filters(&q)?;
    let permit = state.heavy_permit()?;
    let page = state
        .search
        .search(&q.q, filters, cursor, limit.get())
        .await
        .map_err(map_search_err)?;
    let next = page.next.map(SearchOffset::encode_token);
    let owned = state.clone();
    let hits = hydrate_stream(page.hits, move |hit| {
        let owned = owned.clone();
        let uri = hit.uri.clone();
        let nsid = hit.nsid.clone();
        async move {
            let result = hydrate_search_hit(&owned, hit).await;
            drop_unhydratable(HitProvenance::Indexed, &nsid, &uri, result)
        }
    });
    Ok(json_stream::<SearchHitView, _>(
        "hits",
        hits,
        paged_tail(next),
        permit,
    ))
}

fn build_search_filters(q: &SearchQueryParams) -> Result<SearchFilters, XrpcError> {
    let since = q
        .since
        .as_deref()
        .map(parse_rfc3339_seconds)
        .transpose()
        .map_err(|e| XrpcError::InvalidParams(format!("since: {e}")))?;
    let until = q
        .until
        .as_deref()
        .map(parse_rfc3339_seconds)
        .transpose()
        .map_err(|e| XrpcError::InvalidParams(format!("until: {e}")))?;
    if let (Some(s), Some(u)) = (since, until)
        && s > u
    {
        return Err(XrpcError::InvalidParams("since must be <= until".into()));
    }
    Ok(SearchFilters {
        nsid: q.nsid.clone(),
        author: q.author.clone(),
        repo: q.repo.clone(),
        since,
        until,
    })
}

fn parse_rfc3339_seconds(raw: &str) -> Result<i64, String> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.timestamp())
        .map_err(|e| format!("expected RFC3339, got {raw}: {e}"))
}

async fn hydrate_search_hit(
    state: &AppState,
    hit: SearchHit,
) -> Result<Option<SearchHitView>, XrpcError> {
    let SearchHit { uri, nsid, score } = hit;
    let body = resolve_for_view(state, &nsid, uri).await?;
    let record = decode_canon_or_upgrade(&nsid, &body.value, &state.resolver)
        .await
        .map_err(|err| XrpcError::InvalidRecord(err.to_string()))?;
    let Some(value) = SearchableRecord::try_from_record(record) else {
        return Ok(None);
    };
    let Some(value) = value.normalize(&state.resolver).await else {
        return Ok(None);
    };
    Ok(Some(SearchHitView {
        uri: body.uri.clone(),
        cid: Some(body.cid.clone()),
        nsid,
        score,
        value,
    }))
}

fn map_search_err(err: SearchError) -> XrpcError {
    use SearchError as E;
    match err {
        E::Query(e) => XrpcError::InvalidParams(format!("query: {e}")),
        e @ (E::Tantivy(_)
        | E::InvalidUri(_)
        | E::InvalidNsid(_)
        | E::MissingField(_)
        | E::Cancelled(_)) => XrpcError::Internal(format!("search: {e}")),
    }
}

fn map_proxy_error(err: KnotProxyError) -> XrpcError {
    match err {
        KnotProxyError::CircuitOpen => {
            XrpcError::UpstreamUnavailable("knot circuit breaker open".into())
        }
        KnotProxyError::BlockedHost { host, reason } => {
            XrpcError::InvalidRecord(format!("knot host {host} is {reason} address space"))
        }
        KnotProxyError::PlaintextHttp { host } => {
            XrpcError::InvalidRecord(format!("knot host {host} requires https"))
        }
        KnotProxyError::Connect(e) => XrpcError::UpstreamUnavailable(format!("connect: {e}")),
        KnotProxyError::Timeout(e) => {
            XrpcError::UpstreamUnavailable(format!("upstream timeout: {e}"))
        }
        KnotProxyError::Redirect(e) => XrpcError::UpstreamUnavailable(format!("redirect: {e}")),
        KnotProxyError::Transport(e) => XrpcError::UpstreamUnavailable(format!("transport: {e}")),
        KnotProxyError::Upstream(s) => XrpcError::UpstreamUnavailable(format!("status {s}")),
    }
}

fn validate_client_supplied_knot(state: &AppState, host: &KnotHost) -> Result<(), XrpcError> {
    let host_str = || host.url().host_str().unwrap_or_default().to_owned();
    if state.knots.requires_https() && host.url().scheme() != "https" {
        return Err(XrpcError::InvalidParams(format!(
            "knot host {} must be https",
            host_str(),
        )));
    }
    if state.knots.allows_private_hosts() {
        return Ok(());
    }
    match host.private_literal_reason() {
        None => Ok(()),
        Some(reason) => Err(XrpcError::InvalidParams(format!(
            "knot host {} blocked: {} address space",
            host_str(),
            reason,
        ))),
    }
}

async fn resolve_knot_target(
    state: &AppState,
    repo_uri: AtUri<DefaultStr>,
) -> Result<(KnotHost, RepoSlug), XrpcError> {
    let rkey: Option<Rkey<DefaultStr>> = repo_uri.rkey().map(|r| r.clone().into_static());
    let (body, did) = resolve(state, ExpectedNsid::from_static(RepoRecord::NSID), repo_uri).await?;
    let value: Repo<DefaultStr> = serde_json::from_slice(&body.value)
        .map_err(|e| XrpcError::InvalidRecord(format!("decode repo record: {e}")))?;
    let host = KnotHost::parse(value.knot.as_ref())
        .map_err(|e| XrpcError::InvalidRecord(format!("knot field: {e}")))?;
    let name = pick_human_slug(rkey.as_ref(), value.name.as_deref()).ok_or_else(|| {
        XrpcError::InvalidRecord("at-uri missing rkey and record missing name".to_string())
    })?;
    let slug = RepoSlug::new(&did, &name)
        .map_err(|e| XrpcError::InvalidRecord(format!("repo slug: {e}")))?;
    Ok((host, slug))
}

fn pick_human_slug(rkey: Option<&Rkey<DefaultStr>>, name: Option<&str>) -> Option<String> {
    match rkey {
        Some(r) if jacquard_common::types::tid::Tid::new(r.as_ref()).is_ok() => {
            Some(name.unwrap_or(r.as_ref()).to_owned())
        }
        Some(r) => Some(r.as_ref().to_owned()),
        None => name.map(str::to_owned),
    }
}

fn filter_request_headers(
    client: &HeaderMap,
    socket: SocketPeer,
    address: &ClientAddress,
) -> HeaderMap {
    let forwarded = FORWARDED_REQUEST_HEADERS
        .iter()
        .fold(HeaderMap::new(), |mut acc, name| {
            if let Some(value) = client.get(*name) {
                acc.insert((*name).clone(), value.clone());
            }
            acc
        });
    address
        .of(client, socket)
        .into_iter()
        .fold(forwarded, |mut acc, address| {
            acc.insert(X_FORWARDED_FOR.clone(), address);
            acc
        })
}

fn upstream_to_axum(resp: ProxyResponse) -> Response {
    let status = resp.status();
    let upstream_headers = resp.headers().clone();
    let body = Body::from_stream(resp.into_body_stream());
    let mut response = Response::builder()
        .status(status)
        .body(body)
        .expect("response body construction must succeed");
    let response_headers = response.headers_mut();
    PASSTHROUGH_HEADERS.iter().for_each(|name| {
        if let Some(value) = upstream_headers.get(*name) {
            response_headers.insert((*name).clone(), value.clone());
        }
    });
    response
}

async fn dispatch_proxy(
    state: AppState,
    headers: HeaderMap,
    socket: SocketPeer,
    nsid: Nsid<DefaultStr>,
    host: KnotHost,
    params: ProxyParams,
) -> Result<Response, XrpcError> {
    let forward: Vec<(&str, &str)> = params
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let allowed = filter_request_headers(&headers, socket, &state.client_address);
    let upstream = state
        .knots
        .forward(&host, &nsid, &forward, allowed)
        .await
        .map_err(map_proxy_error)?;
    Ok(upstream_to_axum(upstream))
}

fn extract_param(
    params: ProxyParams,
    key: &str,
) -> Result<Option<(String, ProxyParams)>, XrpcError> {
    let (matching, rest): (ProxyParams, ProxyParams) =
        params.into_iter().partition(|(k, _)| k == key);
    match matching.as_slice() {
        [] => Ok(None),
        [_] => Ok(matching.into_iter().next().map(|(_, v)| (v, rest))),
        _ => Err(XrpcError::InvalidParams(format!(
            "{key} parameter must appear at most once, got {}",
            matching.len(),
        ))),
    }
}

async fn proxy_repo_handler(
    state: AppState,
    headers: HeaderMap,
    socket: SocketPeer,
    params: ProxyParams,
    nsid: Nsid<DefaultStr>,
) -> Result<Response, XrpcError> {
    let (repo_raw, rest) = extract_param(params, REPO_PARAM)?
        .ok_or_else(|| XrpcError::InvalidParams("missing repo".into()))?;
    let repo_uri = parse_uri(&repo_raw)?;
    let (host, slug) = resolve_knot_target(&state, repo_uri).await?;
    let forward = rest
        .into_iter()
        .chain(std::iter::once((
            REPO_PARAM.to_owned(),
            slug.as_str().to_owned(),
        )))
        .collect();
    dispatch_proxy(state, headers, socket, nsid, host, forward).await
}

async fn proxy_knot_handler(
    state: AppState,
    headers: HeaderMap,
    socket: SocketPeer,
    params: ProxyParams,
    nsid: Nsid<DefaultStr>,
) -> Result<Response, XrpcError> {
    let (knot_raw, forward) = extract_param(params, KNOT_HOST_PARAM)?
        .ok_or_else(|| XrpcError::InvalidParams("missing knot".into()))?;
    let host =
        KnotHost::parse(&knot_raw).map_err(|e| XrpcError::InvalidParams(format!("knot: {e}")))?;
    validate_client_supplied_knot(&state, &host)?;
    dispatch_proxy(state, headers, socket, nsid, host, forward).await
}
