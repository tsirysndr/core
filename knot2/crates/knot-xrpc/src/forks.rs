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
use knot_pack::{FetchError, HaveOids, PackLimits, UpstreamRefs, WantOids};
use knot_postreceive::{Actor, Ci};
use knot_runtime::{Clock, HttpTransport};
use knot_types::{BranchName, ObjectFormat, Oid, OriginUrl, OwnerDid, RefName, RepoDid, RepoName};

use crate::body::{ForkRef, RemoteRef, SourceUrl};
use crate::error::XrpcError;
use crate::reads::{HostedRepo, require_hosted};
use crate::{XrpcState, decode, ok_empty, run_blocking};

pub(crate) const SYNC_ROUTE: &str = "/xrpc/sh.tangled.repo.forkSync";
pub(crate) const HIDDEN_REF_ROUTE: &str = "/xrpc/sh.tangled.repo.hiddenRef";

#[derive(Clone)]
pub(crate) enum Upstream {
    Local(HostedRepo),
    Remote(Url),
}

fn url_authority(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    }
}

fn path_segments(url: &Url) -> Vec<&str> {
    url.path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default()
}

pub(crate) enum LocalPath {
    Did(RepoDid),
    Named { owner: OwnerDid, name: RepoName },
}

impl LocalPath {
    // The go knot at one point cloned same-host forks like
    // `file:///home/git/<owner-did>/<name>` URLs,
    // so over here in the future what we're gonna do
    // instead of rewriting them all is take the trailing
    // segments, pretend /home/git doesn't exist, and voila,
    // we somewhat know which repo.
    pub(crate) fn parse_trailing(url: &Url) -> Result<Self, &'static str> {
        let segments = path_segments(url);
        match segments.as_slice() {
            [.., owner, name] => match OwnerDid::new(*owner) {
                Ok(owner) => Self::named(owner, name),
                Err(_) => RepoDid::new(*name)
                    .map(Self::Did)
                    .map_err(|_| "path ends in neither /owner-did/name or /repo-did"),
            },
            other => Self::from_segments(other),
        }
    }

    fn from_segments(segments: &[&str]) -> Result<Self, &'static str> {
        match segments {
            [did] => RepoDid::new(*did)
                .map(Self::Did)
                .map_err(|_| "path isn't a DID"),
            [owner, name] => OwnerDid::new(*owner)
                .map_err(|_| "owner segment isn't a DID")
                .and_then(|owner| Self::named(owner, name)),
            _ => Err("path must be /did or /owner/name"),
        }
    }

    fn named(owner: OwnerDid, name: &str) -> Result<Self, &'static str> {
        let name = name.strip_suffix(".git").unwrap_or(name);
        RepoName::new(name)
            .map(|name| Self::Named { owner, name })
            .map_err(|_| "name segment isn't a valid repo name")
    }
}

fn resolve_local<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    path: &LocalPath,
) -> Result<HostedRepo, XrpcError> {
    match path {
        LocalPath::Did(did) => require_hosted(state, did.clone()),
        LocalPath::Named { owner, name } => crate::merge::resolve_by_name(state, owner, name),
    }
}

pub(crate) fn resolve_upstream<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    source: &SourceUrl,
) -> Result<Upstream, XrpcError> {
    let url = source.as_url();
    if url_authority(url) == state.knot_authority() {
        return LocalPath::from_segments(&path_segments(url))
            .map_err(|reason| XrpcError::invalid_request(format!("fork source {reason}")))
            .and_then(|path| resolve_local(state, &path))
            .map(Upstream::Local);
    }
    Ok(Upstream::Remote(url.clone()))
}

enum ForkOrigin {
    Source(SourceUrl),
    File(Url),
}

impl ForkOrigin {
    fn parse(origin: &OriginUrl) -> Result<Self, &'static str> {
        let url = Url::parse(origin.as_str()).map_err(|_| "isn't a valid url")?;
        match url.scheme() {
            "file" => Ok(Self::File(url)),
            "http" | "https" => SourceUrl::from_url(url)
                .map(Self::Source)
                .map_err(|_| "has no host"),
            _ => Err("scheme isn't http, https, or file"),
        }
    }
}

fn resolve_origin<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    origin: &ForkOrigin,
) -> Result<Upstream, XrpcError> {
    match origin {
        ForkOrigin::Source(source) => resolve_upstream(state, source),
        ForkOrigin::File(url) => LocalPath::parse_trailing(url)
            .map_err(|reason| XrpcError::internal(format!("stored fork origin {reason}")))
            .and_then(|path| resolve_local(state, &path))
            .map(Upstream::Local),
    }
}

pub(crate) struct ForkSource {
    pub(crate) origin: SourceUrl,
    pub(crate) upstream: Upstream,
    pub(crate) refs: UpstreamRefs,
}

impl ForkSource {
    pub(crate) async fn resolve<H: HttpTransport, C: Clock>(
        state: &XrpcState<H, C>,
        origin: &SourceUrl,
    ) -> Result<Self, XrpcError> {
        let upstream = resolve_upstream(state, origin)?;
        let prefixes = ["HEAD", "refs/heads/", "refs/tags/"]
            .iter()
            .map(|prefix| prefix.to_string())
            .collect();
        let refs = upstream_refs(state, &upstream, prefixes).await?;
        Ok(Self {
            origin: origin.clone(),
            upstream,
            refs,
        })
    }
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
    repo.set_origin_url(&OriginUrl::new(origin.as_str()))
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
    origin: ForkOrigin,
    haves: Vec<Oid>,
    object_format: ObjectFormat,
}

fn load_fork_state(repo: &Repo) -> Result<ForkState, XrpcError> {
    let origin = repo.origin_url().ok_or_else(|| {
        XrpcError::invalid_request("this repository isn't a fork and has no upstream")
    })?;
    let origin = ForkOrigin::parse(&origin)
        .map_err(|reason| XrpcError::internal(format!("stored fork origin {reason}")))?;
    let haves = repo
        .references()?
        .into_iter()
        .map(|record| record.target)
        .collect();
    Ok(ForkState {
        origin,
        haves,
        object_format: repo.object_format(),
    })
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
    repo_did: &HostedRepo,
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

    let upstream = resolve_origin(state, &fork.origin)?;
    let refs = upstream_refs(state, &upstream, vec![branch.as_str().to_string()]).await?;
    if refs.object_format != fork.object_format {
        return Err(XrpcError::conflict(format!(
            "upstream stores {} objects but this fork stores {}",
            refs.object_format.capability(),
            fork.object_format.capability()
        )));
    }
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
        repo_did.clone().into_did(),
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
    repo: RepoDid,
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
    let repo_did = require_hosted(&state, input.repo)?;
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
                repo: event_repo.into_did(),
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
    repo: RepoDid,
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
    let repo_did = require_hosted(&state, input.repo)?;
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
