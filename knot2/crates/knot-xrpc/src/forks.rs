use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use http::HeaderMap;
use serde::{Deserialize, Serialize};
use url::Url;

use knot_events::Reservation;
use knot_git::{Filter, GitError, Haves, RefUpdate, Repo, Staging, Wants};
use knot_index::Resolved;
use knot_pack::{FetchError, HaveOids, PackLimits, UpstreamRefs, WantOids};
use knot_postreceive::{Actor, Ci};
use knot_runtime::{Clock, HttpTransport};
use knot_types::{BranchName, Oid, OwnerDid, RefName, RepoDid};

use crate::body::{ForkRef, RemoteRef, RepoAtUri, RepoNameArg, Revspec, SourceUrl};
use crate::branches::resolve_at_uri;
use crate::error::XrpcError;
use crate::{XrpcState, decode, ok_empty, run_blocking};

pub(crate) const STATUS_ROUTE: &str = "/xrpc/sh.tangled.repo.forkStatus";
pub(crate) const SYNC_ROUTE: &str = "/xrpc/sh.tangled.repo.forkSync";
pub(crate) const HIDDEN_REF_ROUTE: &str = "/xrpc/sh.tangled.repo.hiddenRef";

#[derive(Clone)]
pub(crate) enum Upstream {
    Local(RepoDid),
    Remote(Url),
}

fn url_authority(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    }
}

fn resolve_local_path<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    url: &Url,
) -> Result<RepoDid, XrpcError> {
    let segments: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();
    match segments.as_slice() {
        [did] => {
            let did = RepoDid::new(*did)
                .map_err(|_| XrpcError::invalid_request("fork source path isn't a DID"))?;
            match state.index.owner_of(&did) {
                Resolved::Ready(Some(_)) => Ok(did),
                Resolved::Ready(None) => Err(XrpcError::not_found(
                    "fork source isn't hosted on this knot",
                )),
                Resolved::Warming => {
                    Err(XrpcError::warming("registry projection is still warming"))
                }
            }
        }
        [owner, name] => {
            let owner = OwnerDid::new(*owner)
                .map_err(|_| XrpcError::invalid_request("fork source owner segment isn't a DID"))?;
            let name = name.strip_suffix(".git").unwrap_or(name);
            crate::merge::resolve_by_name(state, &owner, name)
        }
        _ => Err(XrpcError::invalid_request(
            "fork source path must be /did or /owner/name",
        )),
    }
}

pub(crate) fn resolve_upstream<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    source: &SourceUrl,
) -> Result<Upstream, XrpcError> {
    let url = source.as_url();
    if url_authority(url) == state.knot_authority() {
        return resolve_local_path(state, url).map(Upstream::Local);
    }
    Ok(Upstream::Remote(url.clone()))
}

fn map_fetch(error: FetchError) -> XrpcError {
    match error {
        FetchError::PackTooLarge { limit } => XrpcError::request_too_large(format!(
            "upstream pack exceeds this knot's fork limit of {limit} bytes"
        )),
        FetchError::Pack(inner) => {
            XrpcError::bad_gateway(format!("upstream sent an unusable pack: {inner}"))
        }
        other => XrpcError::bad_gateway(other.to_string()),
    }
}

pub(crate) async fn upstream_refs<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    upstream: &Upstream,
    prefixes: Vec<String>,
) -> Result<UpstreamRefs, XrpcError> {
    match upstream {
        Upstream::Local(did) => {
            let layout = state.layout.clone();
            let did = did.clone();
            run_blocking(move || {
                let repo = layout.open(&did)?;
                let prefixes: Vec<&str> = prefixes.iter().map(String::as_str).collect();
                knot_pack::local_refs(&repo, &prefixes).map_err(XrpcError::from)
            })
            .await
        }
        Upstream::Remote(url) => {
            let prefixes: Vec<&str> = prefixes.iter().map(String::as_str).collect();
            knot_pack::remote_refs(state.git_http.as_ref(), url, &prefixes)
                .await
                .map_err(map_fetch)
        }
    }
}

pub(crate) async fn upstream_pack<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    upstream: &Upstream,
    wants: WantOids,
    haves: HaveOids,
) -> Result<Vec<u8>, XrpcError> {
    let byte_limit = state.byte_limits.fork_pack.get();
    match upstream {
        Upstream::Local(did) => {
            let layout = state.layout.clone();
            let did = did.clone();
            run_blocking(move || {
                let repo = layout.open(&did)?;
                knot_pack::local_pack(&repo, &wants, &haves, byte_limit).map_err(
                    |error| match error {
                        FetchError::PackTooLarge { limit } => {
                            XrpcError::request_too_large(format!(
                                "fork source pack exceeds this knot's fork limit of {limit} bytes"
                            ))
                        }
                        other => XrpcError::internal(other.to_string()),
                    },
                )
            })
            .await
        }
        Upstream::Remote(url) => {
            knot_pack::remote_pack(state.git_http.as_ref(), url, &wants, &haves, byte_limit)
                .await
                .map_err(map_fetch)
        }
    }
}

fn connected(repo: &Repo, wants: Wants<'_>, haves: Haves<'_>) -> Result<bool, XrpcError> {
    repo.select_pack_objects_filtered(wants, haves, Filter::None, knot_pack::selection_budget())
        .map(|selection| selection.send.iter().all(|oid| repo.contains(*oid)))
        .map_err(XrpcError::from)
}

pub(crate) fn populate_fork(
    repo: &Repo,
    refs: &UpstreamRefs,
    pack: &[u8],
    origin: &SourceUrl,
) -> Result<(), XrpcError> {
    knot_pack::ingest_pack(
        &repo.objects_dir(),
        pack,
        &PackLimits::default(),
        repo.object_format().kind(),
    )
    .map_err(|error| XrpcError::bad_gateway(format!("fork source pack is unusable: {error}")))?;
    if !connected(repo, Wants::new(&refs.tips()), Haves::new(&[]))? {
        return Err(XrpcError::bad_gateway(
            "fork source sent an incomplete pack",
        ));
    }
    let creates: Vec<RefUpdate> = refs
        .refs
        .iter()
        .map(|record| RefUpdate::Create {
            name: record.name.clone(),
            new: record.target,
        })
        .collect();
    if !creates.is_empty() {
        repo.update_refs(&creates)?;
    }
    if let Some(head) = refs
        .head_symref
        .as_ref()
        .filter(|head| head.as_str().starts_with("refs/heads/"))
    {
        repo.set_head(head)?;
    }
    repo.set_origin_url(origin.as_str())
        .map_err(XrpcError::from)
}

fn pull_into_live(live: &Repo, pack: &[u8], tip: Oid, haves: &[Oid]) -> Result<(), XrpcError> {
    if live.contains(tip) {
        return Ok(());
    }
    let staging = Staging::new(live)?;
    knot_pack::ingest_pack(
        &staging.repo().objects_dir(),
        pack,
        &PackLimits::default(),
        live.object_format().kind(),
    )
    .map_err(|error| XrpcError::bad_gateway(format!("upstream pack is unusable: {error}")))?;
    if !connected(staging.repo(), Wants::new(&[tip]), Haves::new(haves))? {
        return Err(XrpcError::bad_gateway("upstream sent an incomplete pack"));
    }
    staging.migrate_into(live).map_err(XrpcError::from)
}

const FORCE_REF_ATTEMPTS: usize = 16;

enum ForceStep {
    Done(Option<Reservation>),
    Retry,
}

fn force_ref(
    repo: &Repo,
    name: &RefName,
    new: Oid,
    reserve: &dyn Fn() -> Reservation,
) -> Result<Option<Reservation>, XrpcError> {
    fn attempt(
        repo: &Repo,
        name: &RefName,
        new: Oid,
        reserve: &dyn Fn() -> Reservation,
    ) -> Result<ForceStep, XrpcError> {
        let current = repo.find_ref(name)?;
        let update = match current {
            Some(old) if old == new => return Ok(ForceStep::Done(None)),
            Some(old) => RefUpdate::Update {
                name: name.clone(),
                old,
                new,
            },
            None => RefUpdate::Create {
                name: name.clone(),
                new,
            },
        };
        match repo.update_ref_sealed(&update, reserve) {
            Ok(reservation) => Ok(ForceStep::Done(Some(reservation))),
            Err(GitError::Reference { .. } | GitError::AtomicRefs(_)) => Ok(ForceStep::Retry),
            Err(other) => Err(XrpcError::internal(other.to_string())),
        }
    }

    (0..FORCE_REF_ATTEMPTS)
        .find_map(|_| match attempt(repo, name, new, reserve) {
            Ok(ForceStep::Done(reservation)) => Some(Ok(reservation)),
            Ok(ForceStep::Retry) => None,
            Err(error) => Some(Err(error)),
        })
        .unwrap_or_else(|| Err(XrpcError::conflict("ref moved during pull, retry")))
}

const FORK_DENIED: &str = "only repository owner or a collaborator may operate on this fork";

struct ForkState {
    origin: SourceUrl,
    haves: Vec<Oid>,
}

fn load_fork_state(repo: &Repo) -> Result<ForkState, XrpcError> {
    let origin = repo.origin_url().ok_or_else(|| {
        XrpcError::invalid_request("this repository isn't a fork and has no upstream")
    })?;
    let origin = SourceUrl::parse(&origin)
        .map_err(|reason| XrpcError::internal(format!("stored fork origin: {reason}")))?;
    let haves = repo
        .references()?
        .into_iter()
        .map(|record| record.target)
        .collect();
    Ok(ForkState { origin, haves })
}

pub(crate) struct SyncResult {
    old: Option<Oid>,
    new: Oid,
    reservation: Option<Reservation>,
    lfs_missing: Vec<knot_lfs::LfsOid>,
}

#[derive(Clone, Copy)]
struct SourceRef<'a>(&'a RefName);

impl<'a> SourceRef<'a> {
    fn new(reference: &'a RefName) -> Self {
        Self(reference)
    }

    fn get(self) -> &'a RefName {
        self.0
    }
}

#[derive(Clone, Copy)]
struct TargetRef<'a>(&'a RefName);

impl<'a> TargetRef<'a> {
    fn new(reference: &'a RefName) -> Self {
        Self(reference)
    }

    fn get(self) -> &'a RefName {
        self.0
    }
}

async fn pull_upstream_branch<H: HttpTransport, C: Clock>(
    state: &Arc<XrpcState<H, C>>,
    repo_did: &RepoDid,
    source: SourceRef<'_>,
    target: TargetRef<'_>,
) -> Result<SyncResult, XrpcError> {
    let branch = source.get();
    let target = target.get();
    let layout = state.layout.clone();
    let opened = repo_did.clone();
    let fork = run_blocking(move || {
        let repo = layout.open(&opened)?;
        load_fork_state(&repo)
    })
    .await?;

    let upstream = resolve_upstream(state, &fork.origin)?;
    let refs = upstream_refs(state, &upstream, vec![branch.as_str().to_string()]).await?;
    let tip = refs
        .find(branch)
        .ok_or_else(|| XrpcError::not_found("upstream repository doesn't have that branch"))?;
    let pack = upstream_pack(
        state,
        &upstream,
        WantOids::new(vec![tip]),
        HaveOids::new(fork.haves.clone()),
    )
    .await?;

    let layout = state.layout.clone();
    let opened = repo_did.clone();
    let haves = fork.haves.clone();
    run_blocking(move || {
        let repo = layout.open(&opened)?;
        pull_into_live(&repo, &pack, tip, &fork.haves)
    })
    .await?;

    let lfs_missing = crate::lfs::mirror_fork_objects(
        Arc::clone(state),
        upstream,
        repo_did.clone(),
        WantOids::new(vec![tip]),
        HaveOids::new(haves),
    )
    .await?;

    let layout = state.layout.clone();
    let opened = repo_did.clone();
    let target = target.clone();
    let events = Arc::clone(&state.events);
    run_blocking(move || {
        let repo = layout.open(&opened)?;
        let old = repo.find_ref(&target)?;
        let reservation = force_ref(&repo, &target, tip, &|| events.reserve())?;
        Ok(SyncResult {
            old,
            new: tip,
            reservation,
            lfs_missing,
        })
    })
    .await
}

#[derive(Deserialize)]
struct ForkSyncInput {
    did: OwnerDid,
    name: RepoNameArg,
    branch: BranchName,
}

pub(crate) async fn fork_sync<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    headers: HeaderMap,
    method: crate::Method,
    body: Bytes,
) -> Result<Response, XrpcError> {
    let actor = state.authenticate(&headers, &method).await?;
    let input: ForkSyncInput = decode(&body)?;
    let repo_did = crate::merge::resolve_by_name(&state, &input.did, input.name.as_str())?;
    crate::authorize_push(&state, &actor, &repo_did, FORK_DENIED).await?;
    let branch = input.branch.head_ref();
    let sync = pull_upstream_branch(
        &state,
        &repo_did,
        SourceRef::new(&branch),
        TargetRef::new(&branch),
    )
    .await?;
    if let Some(reservation) = sync.reservation {
        let owner = crate::current_owner(&state, &repo_did);
        let layout = state.layout.clone();
        let languages_push_budget = state.budgets.languages_push;
        let catalog = Arc::clone(&state.catalog);
        let event_repo = repo_did.clone();
        let (old, new) = (sync.old, sync.new);
        if let Err(error) = run_blocking(move || -> Result<(), XrpcError> {
            let repo = layout.open(&event_repo)?;
            let update = match old {
                Some(old) => RefUpdate::Update {
                    name: branch,
                    old,
                    new,
                },
                None => RefUpdate::Create { name: branch, new },
            };
            let post_actor = Actor {
                committer: actor,
                owner,
                repo: event_repo,
            };
            knot_postreceive::post_receive(
                &repo,
                &post_actor,
                vec![(update, reservation)],
                &Ci::Skip,
                &knot_types::PushOptions::default(),
                None,
                languages_push_budget,
                &catalog.push,
            );
            Ok(())
        })
        .await
        {
            tracing::warn!(repo = repo_did.as_str(), %error, "post-receive after fork sync failed");
        }
    }
    match sync.lfs_missing.is_empty() {
        true => Ok(ok_empty()),
        false => Ok((
            http::StatusCode::OK,
            Json(serde_json::json!({
                "lfsMissing": sync.lfs_missing.iter().map(|oid| oid.as_str()).collect::<Vec<_>>(),
            })),
        )
            .into_response()),
    }
}

#[derive(Deserialize)]
struct HiddenRefInput {
    repo: RepoAtUri,
    #[serde(rename = "forkRef")]
    fork_ref: ForkRef,
    #[serde(rename = "remoteRef")]
    remote_ref: RemoteRef,
}

#[derive(Serialize)]
struct HiddenRefOutput {
    success: bool,
    #[serde(rename = "ref")]
    ref_name: RefName,
    #[serde(rename = "lfsMissing", skip_serializing_if = "Vec::is_empty")]
    lfs_missing: Vec<knot_lfs::LfsOid>,
}

pub(crate) async fn hidden_ref<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    headers: HeaderMap,
    method: crate::Method,
    body: Bytes,
) -> Result<Response, XrpcError> {
    let actor = state.authenticate(&headers, &method).await?;
    let input: HiddenRefInput = decode(&body)?;
    let repo_did = resolve_at_uri(&state, input.repo.at_uri())?;
    crate::authorize_push(&state, &actor, &repo_did, FORK_DENIED).await?;
    let branch = input.remote_ref.head_ref();
    let target = input
        .fork_ref
        .hidden_ref(&input.remote_ref)
        .ok_or_else(|| {
            XrpcError::invalid_request("forkRef and remoteRef don't form a valid ref")
        })?;
    let sync = pull_upstream_branch(
        &state,
        &repo_did,
        SourceRef::new(&branch),
        TargetRef::new(&target),
    )
    .await?;
    Ok((
        http::StatusCode::OK,
        Json(HiddenRefOutput {
            success: true,
            ref_name: target,
            lfs_missing: sync.lfs_missing,
        }),
    )
        .into_response())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForkStatus {
    UpToDate,
    FastForwardable,
    Conflict,
}

impl ForkStatus {
    fn code(self) -> u8 {
        match self {
            ForkStatus::UpToDate => 0,
            ForkStatus::FastForwardable => 1,
            ForkStatus::Conflict => 2,
        }
    }
}

#[derive(Deserialize)]
struct ForkStatusInput {
    did: OwnerDid,
    name: Option<RepoNameArg>,
    #[serde(default, deserialize_with = "crate::body::optional_source_url")]
    source: Option<SourceUrl>,
    branch: Revspec,
    #[serde(rename = "hiddenRef")]
    hidden_ref: Revspec,
}

#[derive(Serialize)]
struct ForkStatusOutput {
    status: u8,
}

fn source_basename(source: &Url) -> Option<String> {
    source
        .path_segments()
        .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
        .map(str::to_string)
}

pub(crate) async fn fork_status<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    headers: HeaderMap,
    method: crate::Method,
    body: Bytes,
) -> Result<Response, XrpcError> {
    let actor = state.authenticate(&headers, &method).await?;
    let input: ForkStatusInput = decode(&body)?;
    let name = input
        .name
        .as_ref()
        .map(|name| name.as_str().to_string())
        .or_else(|| {
            input
                .source
                .as_ref()
                .and_then(|source| source_basename(source.as_url()))
        })
        .ok_or_else(|| {
            XrpcError::invalid_request("neither name nor a source url with path was supplied")
        })?;
    let repo_did = crate::merge::resolve_by_name(&state, &input.did, &name)?;
    crate::authorize_push(&state, &actor, &repo_did, FORK_DENIED).await?;

    let layout = state.layout.clone();
    let status = run_blocking(move || {
        let repo = layout.open(&repo_did)?;
        let fork = repo
            .resolve_revision(input.branch.as_str())
            .ok_or_else(|| {
                XrpcError::invalid_request(format!(
                    "cannot resolve revision {}",
                    input.branch.as_str()
                ))
            })
            .and_then(|oid| {
                repo.peel_to_commit(oid)
                    .map_err(|error| XrpcError::invalid_request(error.to_string()))
            })?;
        let source = repo
            .resolve_revision(input.hidden_ref.as_str())
            .ok_or_else(|| {
                XrpcError::invalid_request(format!(
                    "cannot resolve revision {}",
                    input.hidden_ref.as_str()
                ))
            })
            .and_then(|oid| {
                repo.peel_to_commit(oid)
                    .map_err(|error| XrpcError::invalid_request(error.to_string()))
            })?;
        if fork == source {
            return Ok(ForkStatus::UpToDate);
        }
        let base = repo.merge_base(fork, source)?;
        Ok(match base {
            Some(base) if base == fork => ForkStatus::FastForwardable,
            Some(base) if base == source => ForkStatus::UpToDate,
            _ => ForkStatus::Conflict,
        })
    })
    .await?;

    Ok((
        http::StatusCode::OK,
        Json(ForkStatusOutput {
            status: status.code(),
        }),
    )
        .into_response())
}
