use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use knot_lfs::{
    BATCH_MEDIA_TYPE, BatchAction, BatchActions, BatchObject, BatchObjectError, BatchOperation,
    BatchRequest, BatchResponse, BatchResponseObject, ClaimedSize, LfsError, LfsHandle, LfsOid,
    LfsSize, LfsStore, MAX_BATCH_OBJECTS, UploadAdmission,
};
use knot_pack::{HaveOids, SocketPeer, WantOids};
use knot_runtime::{Clock, HttpTransport};
use knot_types::{AccountDid, HttpStatus, KnotServiceUrl, RepoDid};
use serde_json::json;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tower::ServiceExt;
use tower_http::services::ServeFile;
use url::Url;

use crate::XrpcState;
use crate::forks::Upstream;

pub const MAX_BATCH_BYTES: usize = 1024 * 1024;

const IMMUTABLE_CACHE: &str = "public, max-age=31536000, immutable";

const READINESS_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

struct Readiness {
    result: watch::Sender<Option<bool>>,
    probing: Mutex<bool>,
}

pub struct LfsWeb {
    pub handle: LfsHandle,
    downloads: Arc<Semaphore>,
    readiness: Arc<Readiness>,
}

impl LfsWeb {
    pub fn new(handle: LfsHandle, max_downloads: usize) -> Self {
        let (result, _rx) = watch::channel(None);
        Self {
            handle,
            downloads: Arc::new(Semaphore::new(max_downloads)),
            readiness: Arc::new(Readiness {
                result,
                probing: Mutex::new(false),
            }),
        }
    }

    pub async fn ready(&self) -> bool {
        let mut rx = self.readiness.result.subscribe();
        let launch = {
            let mut probing = self.readiness.probing.lock().unwrap();
            match *probing {
                false => {
                    *probing = true;
                    self.readiness.result.send_replace(None);
                    true
                }
                true => false,
            }
        };
        if launch {
            Self::spawn_probe(Arc::clone(&self.readiness), Arc::clone(&self.handle.store));
        }
        let settled = async {
            match rx.wait_for(Option::is_some).await {
                Ok(seen) => seen.unwrap_or(false),
                Err(_) => false,
            }
        };
        tokio::time::timeout(READINESS_PROBE_TIMEOUT + Duration::from_secs(1), settled)
            .await
            .unwrap_or(false)
    }

    fn spawn_probe(readiness: Arc<Readiness>, store: Arc<knot_lfs::DiskStore>) {
        tokio::spawn(async move {
            let mut guard = ProbeGuard {
                readiness,
                outcome: false,
            };
            let joined = tokio::task::spawn_blocking(move || store.probe_ready()).await;
            let ok = matches!(joined, Ok(Ok(())));
            if !ok {
                tracing::warn!("lfs store readiness probe failed, reporting unready");
            }
            guard.outcome = ok;
        });
    }
}

struct ProbeGuard {
    readiness: Arc<Readiness>,
    outcome: bool,
}

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        *self.readiness.probing.lock().unwrap() = false;
        self.readiness.result.send_replace(Some(self.outcome));
    }
}

pub(crate) fn routes<H: HttpTransport, C: Clock>() -> Router<Arc<XrpcState<H, C>>> {
    let batch = Router::new()
        .route(
            "/{did}/{name}/info/lfs/objects/batch",
            post(batch_named::<H, C>),
        )
        .route("/{did}/info/lfs/objects/batch", post(batch_did::<H, C>))
        .layer(DefaultBodyLimit::max(MAX_BATCH_BYTES));
    let objects = Router::new()
        .route(
            "/{did}/{name}/info/lfs/objects/{oid}",
            get(object_named::<H, C>).put(object_upload_named::<H, C>),
        )
        .route(
            "/{did}/info/lfs/objects/{oid}",
            get(object_did::<H, C>).put(object_upload_did::<H, C>),
        )
        .layer(DefaultBodyLimit::disable());
    batch.merge(objects)
}

fn fail(status: StatusCode, message: &str) -> Response {
    (
        status,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static(BATCH_MEDIA_TYPE),
        )],
        json!({ "message": message }).to_string(),
    )
        .into_response()
}

fn not_enabled() -> Response {
    fail(StatusCode::NOT_FOUND, "LFS isn't enabled on this knot")
}

fn lfs_error(error: crate::XrpcError) -> Box<Response> {
    Box::new(fail(error.status(), &error.to_string()))
}

fn resolve_did<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    segment: &crate::RepoDidSegment,
) -> Result<RepoDid, Box<Response>> {
    crate::resolve_repo_did(state, segment).map_err(lfs_error)
}

async fn resolve_named<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    owner: &crate::OwnerSegment,
    name: &crate::RepoNameSegment,
) -> Result<RepoDid, Box<Response>> {
    crate::resolve_repo_named(state, owner, name)
        .await
        .map_err(lfs_error)
}

#[derive(serde::Deserialize)]
#[serde(transparent)]
struct OidSegment(String);

impl OidSegment {
    fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(serde::Deserialize)]
struct RepoObjectParams {
    did: crate::OwnerSegment,
    name: crate::RepoNameSegment,
    oid: OidSegment,
}

#[derive(serde::Deserialize)]
struct DidObjectParams {
    did: crate::RepoDidSegment,
    oid: OidSegment,
}

async fn batch_named<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    Path(crate::RepoPathParams { did, name }): Path<crate::RepoPathParams>,
    peer: SocketPeer,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let repo = match resolve_named(&state, &did, &name).await {
        Ok(repo) => repo,
        Err(response) => return *response,
    };
    serve_batch(
        &state,
        repo,
        &format!("{}/{}", did.as_str(), name.as_str()),
        peer,
        &headers,
        body,
    )
    .await
}

async fn batch_did<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    Path(did): Path<crate::RepoDidSegment>,
    peer: SocketPeer,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let repo = match resolve_did(&state, &did) {
        Ok(repo) => repo,
        Err(response) => return *response,
    };
    serve_batch(&state, repo, did.as_str(), peer, &headers, body).await
}

async fn serve_batch<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    repo: RepoDid,
    path_prefix: &str,
    peer: SocketPeer,
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let Some(lfs) = state.lfs.as_ref() else {
        return not_enabled();
    };
    let body = match body {
        Ok(body) => body,
        Err(rejection) => return fail(rejection.status(), &rejection.body_text()),
    };
    let request: BatchRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return fail(
                StatusCode::UNPROCESSABLE_ENTITY,
                &format!("invalid batch request: {error}"),
            );
        }
    };
    if request.objects.len() > MAX_BATCH_OBJECTS {
        return fail(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("batch exceeds {MAX_BATCH_OBJECTS} objects"),
        );
    }
    if !request.transfers.is_empty()
        && !request
            .transfers
            .iter()
            .any(knot_lfs::TransferAdapter::is_basic)
    {
        return fail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "no mutually supported transfer adapter, this server serves basic",
        );
    }
    if let Some(algo) = &request.hash_algo
        && !algo.is_sha256()
    {
        return fail(
            StatusCode::CONFLICT,
            &format!("unsupported hash algorithm {:?}", algo.as_str()),
        );
    }
    let base = &state.knot_service_url;
    let objects: Result<Vec<BatchResponseObject>, Box<Response>> = match request.operation {
        BatchOperation::Upload => match authorized_pusher(state, peer, headers, &repo).await {
            Ok(_actor) => match probe_all(lfs, &repo, &request.objects).await {
                Ok(stored) => request
                    .objects
                    .iter()
                    .zip(stored)
                    .map(|(object, present)| upload_action(base, path_prefix, object, present))
                    .collect(),
                Err(response) => Err(response),
            },
            Err(response) => return *response,
        },
        BatchOperation::Download => match probe_all(lfs, &repo, &request.objects).await {
            Ok(stored) => request
                .objects
                .iter()
                .zip(stored)
                .map(|(object, stored)| downloadable(base, path_prefix, object, stored))
                .collect(),
            Err(response) => Err(response),
        },
    };
    let objects = match objects {
        Ok(objects) => objects,
        Err(response) => return *response,
    };
    let response = BatchResponse {
        transfer: knot_lfs::TransferAdapter::Basic,
        objects,
        hash_algo: Some(knot_lfs::HashAlgo::Sha256),
    };
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static(BATCH_MEDIA_TYPE),
        )],
        serde_json::to_string(&response).expect("batch response always serializes"),
    )
        .into_response()
}

async fn authorized_pusher<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    peer: SocketPeer,
    headers: &HeaderMap,
    repo: &RepoDid,
) -> Result<AccountDid, Box<Response>> {
    crate::authenticate_and_authorize_push(
        state,
        peer,
        headers,
        repo,
        "you aren't authorized to push to this repository",
    )
    .await
    .map_err(|error| Box::new(challenge(error)))
}

fn challenge(error: crate::XrpcError) -> Response {
    if error.status() == StatusCode::UNAUTHORIZED {
        unauthorized(&error.to_string())
    } else {
        error.into_response()
    }
}

fn unauthorized(message: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [
            (header::WWW_AUTHENTICATE, crate::BASIC_CHALLENGE),
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static(BATCH_MEDIA_TYPE),
            ),
        ],
        json!({ "message": message }).to_string(),
    )
        .into_response()
}

fn upload_action(
    base: &KnotServiceUrl,
    path_prefix: &str,
    object: &BatchObject,
    present: Option<LfsSize>,
) -> Result<BatchResponseObject, Box<Response>> {
    Ok(match present {
        Some(size) => BatchResponseObject {
            oid: object.oid.clone(),
            size: ClaimedSize::new(size.get()),
            authenticated: Some(true),
            actions: None,
            error: None,
        },
        None => BatchResponseObject {
            oid: object.oid.clone(),
            size: object.size,
            // I know what you're thinking about putting `authenticated: true`,
            // but trust me TM git-lfs thinks
            // "the href already has credentials on it"
            // and omits the Authorization header from the following PUT req.
            // PUT needs auth so the client would 401, re-run the batch,
            // repeat.
            //
            // Having this be `None` makes git-lfs re-send the header it
            // used on the batch-call in the first place.
            authenticated: None,
            actions: Some(BatchActions {
                download: None,
                upload: Some(BatchAction {
                    href: object_href(base, path_prefix, &object.oid)?,
                }),
            }),
            error: None,
        },
    })
}

async fn probe_all(
    lfs: &LfsWeb,
    repo: &RepoDid,
    objects: &[BatchObject],
) -> Result<Vec<Option<LfsSize>>, Box<Response>> {
    let store = Arc::clone(&lfs.handle.store);
    let target = repo.clone();
    let oids: Vec<LfsOid> = objects.iter().map(|object| object.oid.clone()).collect();
    tokio::task::spawn_blocking(move || {
        oids.iter()
            .map(|oid| store.probe(&target, oid))
            .collect::<Result<Vec<_>, _>>()
    })
    .await
    .map_err(|_| {
        Box::new(fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "store probe failed",
        ))
    })?
    .map_err(|error| {
        tracing::warn!(repo = repo.as_str(), %error, "lfs store probe failed");
        Box::new(fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "store probe failed",
        ))
    })
}

fn downloadable(
    base: &KnotServiceUrl,
    path_prefix: &str,
    object: &BatchObject,
    stored: Option<LfsSize>,
) -> Result<BatchResponseObject, Box<Response>> {
    Ok(match stored {
        Some(size) => BatchResponseObject {
            oid: object.oid.clone(),
            size: ClaimedSize::new(size.get()),
            authenticated: Some(true),
            actions: Some(BatchActions {
                download: Some(BatchAction {
                    href: object_href(base, path_prefix, &object.oid)?,
                }),
                upload: None,
            }),
            error: None,
        },
        None => BatchResponseObject {
            oid: object.oid.clone(),
            size: object.size,
            authenticated: None,
            actions: None,
            error: Some(BatchObjectError {
                code: HttpStatus::new(404),
                message: "object not found".to_string(),
            }),
        },
    })
}

fn object_href(
    base: &KnotServiceUrl,
    path_prefix: &str,
    oid: &LfsOid,
) -> Result<Url, Box<Response>> {
    Url::parse(&format!(
        "{}/{path_prefix}/info/lfs/objects/{oid}",
        base.as_str()
    ))
    .map_err(|error| {
        Box::new(fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("cannot derive object href: {error}"),
        ))
    })
}

async fn object_named<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    Path(RepoObjectParams { did, name, oid }): Path<RepoObjectParams>,
    request: Request,
) -> Response {
    let repo = match resolve_named(&state, &did, &name).await {
        Ok(repo) => repo,
        Err(response) => return *response,
    };
    serve_object(&state, repo, &oid, request).await
}

async fn object_did<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    Path(DidObjectParams { did, oid }): Path<DidObjectParams>,
    request: Request,
) -> Response {
    let repo = match resolve_did(&state, &did) {
        Ok(repo) => repo,
        Err(response) => return *response,
    };
    serve_object(&state, repo, &oid, request).await
}

async fn serve_object<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    repo: RepoDid,
    oid_raw: &OidSegment,
    request: Request,
) -> Response {
    let Some(lfs) = state.lfs.as_ref() else {
        return not_enabled();
    };
    let Ok(oid) = LfsOid::new(oid_raw.as_str()) else {
        return fail(StatusCode::NOT_FOUND, "object not found");
    };
    let located = {
        let store = Arc::clone(&lfs.handle.store);
        let target = repo.clone();
        let oid = oid.clone();
        tokio::task::spawn_blocking(move || store.object_file(&target, &oid)).await
    };
    let (size, path) = match located {
        Ok(Ok(Some((size, path)))) => (size, path),
        Ok(Ok(None)) => return fail(StatusCode::NOT_FOUND, "object not found"),
        Ok(Err(error)) => {
            tracing::warn!(repo = repo.as_str(), oid = oid.as_str(), %error, "lfs store read failed");
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "store read failed");
        }
        Err(_) => return fail(StatusCode::INTERNAL_SERVER_ERROR, "store read failed"),
    };
    let etag = format!("\"{oid}\"");
    if client_holds_current(request.headers().get(header::IF_NONE_MATCH), &etag) {
        return not_modified(&etag);
    }
    let request = honor_if_range(request, &etag);
    let permit = match Arc::clone(&lfs.downloads).acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => {
            return fail(
                StatusCode::SERVICE_UNAVAILABLE,
                "server is shutting down, retry shortly",
            );
        }
    };
    let served = ServeFile::new(path)
        .oneshot(request)
        .await
        .map(|response| response.map(Body::new));
    let mut response = match served {
        Ok(response) => response,
        Err(error) => match error {},
    };
    tracing::info!(
        repo = repo.as_str(),
        oid = oid.as_str(),
        size = size.get(),
        status = response.status().as_u16(),
        "lfs object served over http"
    );
    if response.status().is_success() {
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(IMMUTABLE_CACHE),
        );
        if let Ok(value) = HeaderValue::from_str(&etag) {
            headers.insert(header::ETAG, value);
        }
    }
    response.map(|body| {
        Body::new(PermitBody {
            body,
            _permit: permit,
        })
    })
}

async fn object_upload_named<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    Path(RepoObjectParams { did, name, oid }): Path<RepoObjectParams>,
    peer: SocketPeer,
    request: Request,
) -> Response {
    let repo = match resolve_named(&state, &did, &name).await {
        Ok(repo) => repo,
        Err(response) => return *response,
    };
    serve_object_upload(&state, repo, &oid, peer, request).await
}

async fn object_upload_did<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    Path(DidObjectParams { did, oid }): Path<DidObjectParams>,
    peer: SocketPeer,
    request: Request,
) -> Response {
    let repo = match resolve_did(&state, &did) {
        Ok(repo) => repo,
        Err(response) => return *response,
    };
    serve_object_upload(&state, repo, &oid, peer, request).await
}

async fn serve_object_upload<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    repo: RepoDid,
    oid_raw: &OidSegment,
    peer: SocketPeer,
    request: Request,
) -> Response {
    let Some(lfs) = state.lfs.as_ref() else {
        return not_enabled();
    };
    let Ok(oid) = LfsOid::new(oid_raw.as_str()) else {
        return fail(StatusCode::NOT_FOUND, "object not found");
    };
    if let Err(response) = authorized_pusher(state, peer, request.headers(), &repo).await {
        return *response;
    }
    let Some(size) = content_length(request.headers()) else {
        return fail(
            StatusCode::LENGTH_REQUIRED,
            "content-length is required for an lfs object upload",
        );
    };
    let permit = match lfs.handle.admission.admit(size) {
        Ok(permit) => permit,
        Err(error) => return store_fault(&repo, &oid, error),
    };
    let store = Arc::clone(&lfs.handle.store);
    let target = repo.clone();
    let object = oid.clone();
    let landed = {
        use futures::TryStreamExt;
        let reader = tokio_util::io::StreamReader::new(
            request
                .into_body()
                .into_data_stream()
                .map_err(std::io::Error::other),
        );
        tokio::task::spawn_blocking(move || {
            let mut body = tokio_util::io::SyncIoBridge::new(reader);
            let outcome = store.put(&target, &object, size, &mut body);
            drop(permit);
            outcome
        })
        .await
    };
    match landed {
        Ok(Ok(())) => {
            tracing::info!(
                repo = repo.as_str(),
                oid = oid.as_str(),
                size = size.get(),
                "lfs object stored over http"
            );
            StatusCode::OK.into_response()
        }
        Ok(Err(error)) => store_fault(&repo, &oid, error),
        Err(_) => fail(StatusCode::INTERNAL_SERVER_ERROR, "upload task died"),
    }
}

fn content_length(headers: &HeaderMap) -> Option<ClaimedSize> {
    headers
        .get(header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(ClaimedSize::new)
}

fn store_fault(repo: &RepoDid, oid: &LfsOid, error: LfsError) -> Response {
    let status = match &error {
        LfsError::HashMismatch { .. } | LfsError::SizeMismatch { .. } => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        LfsError::SizeLimitExceeded { .. } => StatusCode::PAYLOAD_TOO_LARGE,
        LfsError::FreeSpaceDenied { .. } => StatusCode::INSUFFICIENT_STORAGE,
        LfsError::BodyRead { .. } => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    if status.is_server_error() {
        tracing::warn!(repo = repo.as_str(), oid = oid.as_str(), %error, "lfs object upload failed");
    }
    fail(status, &error.to_string())
}

fn client_holds_current(if_none_match: Option<&HeaderValue>, etag: &str) -> bool {
    if_none_match
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .map(str::trim)
                .any(|candidate| candidate == "*" || candidate.trim_start_matches("W/") == etag)
        })
}

fn not_modified(etag: &str) -> Response {
    let mut response = StatusCode::NOT_MODIFIED.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(IMMUTABLE_CACHE),
    );
    if let Ok(value) = HeaderValue::from_str(etag) {
        headers.insert(header::ETAG, value);
    }
    response
}

fn honor_if_range(mut request: Request, etag: &str) -> Request {
    let Some(if_range) = request.headers().get(header::IF_RANGE) else {
        return request;
    };
    let matches = if_range
        .to_str()
        .map(|value| value == etag)
        .unwrap_or(false);
    let headers = request.headers_mut();
    headers.remove(header::IF_RANGE);
    if !matches {
        headers.remove(header::RANGE);
    }
    request
}

pub(crate) fn mirror_fork_objects<H: HttpTransport, C: Clock>(
    state: Arc<XrpcState<H, C>>,
    upstream: Upstream,
    fork: RepoDid,
    wants: WantOids,
    haves: HaveOids,
) -> futures::future::BoxFuture<'static, Result<Vec<LfsOid>, crate::XrpcError>> {
    Box::pin(mirror_fork_objects_inner(
        state, upstream, fork, wants, haves,
    ))
}

async fn mirror_fork_objects_inner<H: HttpTransport, C: Clock>(
    state: Arc<XrpcState<H, C>>,
    upstream: Upstream,
    fork: RepoDid,
    wants: WantOids,
    haves: HaveOids,
) -> Result<Vec<LfsOid>, crate::XrpcError> {
    let state = &state;
    let upstream = &upstream;
    let fork = &fork;
    let Some(lfs) = state.lfs.as_ref() else {
        return Ok(Vec::new());
    };
    let store = Arc::clone(&lfs.handle.store);
    let admission = Arc::clone(&lfs.handle.admission);

    let needed = {
        let layout = state.layout.clone();
        let fork = fork.clone();
        let store = Arc::clone(&store);
        tokio::task::spawn_blocking(move || -> Result<Vec<(LfsOid, ClaimedSize)>, String> {
            let repo = layout.open(&fork).map_err(|error| error.to_string())?;
            knot_lfs::scan_pointers(&repo, wants.wants(), haves.haves())
                .map_err(|error| error.to_string())?
                .into_iter()
                .map(|(oid, size)| match store.probe(&fork, &oid) {
                    Ok(None) => Ok(Some((oid, size))),
                    Ok(Some(_)) => Ok(None),
                    Err(fault) => Err(fault.to_string()),
                })
                .filter_map(Result::transpose)
                .collect()
        })
        .await
    };
    let needed = match needed {
        Ok(Ok(needed)) => needed,
        Ok(Err(fault)) => {
            tracing::warn!(
                repo = fork.as_str(),
                fault,
                "lfs pointer scan failed on fork"
            );
            return Err(crate::XrpcError::internal("lfs pointer scan failed"));
        }
        Err(_) => {
            return Err(crate::XrpcError::internal("lfs pointer scan task died"));
        }
    };
    if needed.is_empty() {
        return Ok(Vec::new());
    }

    let missing = match upstream {
        Upstream::Local(source) => {
            let source = source.clone();
            let fork = fork.clone();
            let store = Arc::clone(&store);
            let admission = Arc::clone(&admission);
            tokio::task::spawn_blocking(move || {
                needed
                    .into_iter()
                    .filter_map(|(oid, size)| {
                        let copied = admission.admit(size).and_then(|_permit| {
                            store
                                .read(&source, &oid)
                                .and_then(|mut body| store.put(&fork, &oid, size, &mut body))
                        });
                        match copied {
                            Ok(()) => None,
                            Err(fault) => {
                                tracing::warn!(
                                    source = source.as_str(),
                                    oid = oid.as_str(),
                                    %fault,
                                    "lfs fork copy skipped an object"
                                );
                                Some(oid)
                            }
                        }
                    })
                    .collect()
            })
            .await
            .map_err(|_| crate::XrpcError::internal("lfs fork copy task died"))?
        }
        Upstream::Remote(url) => match remote_batch_url(url) {
            Some(batch_url) => {
                use futures::StreamExt;
                let chunks: Vec<Vec<(LfsOid, ClaimedSize)>> = needed
                    .chunks(REMOTE_BATCH_CHUNK)
                    .map(<[(LfsOid, ClaimedSize)]>::to_vec)
                    .collect();
                futures::stream::iter(chunks)
                    .then(|chunk| {
                        fetch_remote_chunk(
                            Arc::clone(state),
                            Arc::clone(&store),
                            Arc::clone(&admission),
                            fork.clone(),
                            batch_url.clone(),
                            chunk,
                        )
                    })
                    .concat()
                    .await
            }
            None => needed.into_iter().map(|(oid, _)| oid).collect(),
        },
    };
    if !missing.is_empty() {
        tracing::warn!(
            repo = fork.as_str(),
            count = missing.len(),
            "fork upstream couldn't serve every referenced lfs object"
        );
    }
    Ok(missing)
}

const REMOTE_BATCH_CHUNK: usize = 100;

fn remote_batch_url(origin: &Url) -> Option<Url> {
    let mut origin = origin.clone();
    origin.set_query(None);
    origin.set_fragment(None);
    let base = origin.as_str().trim_end_matches('/');
    let base = match base.ends_with(".git") {
        true => base.to_string(),
        false => format!("{base}.git"),
    };
    Url::parse(&format!("{base}/info/lfs/objects/batch")).ok()
}

async fn fetch_remote_chunk<H: HttpTransport, C: Clock>(
    state: Arc<XrpcState<H, C>>,
    store: Arc<knot_lfs::DiskStore>,
    admission: Arc<knot_lfs::StoreAdmission>,
    fork: RepoDid,
    batch_url: Url,
    chunk: Vec<(LfsOid, ClaimedSize)>,
) -> Vec<LfsOid> {
    use futures::StreamExt;
    let all_missing = || chunk.iter().map(|(oid, _)| oid.clone()).collect::<Vec<_>>();
    let request_body = BatchRequest {
        operation: BatchOperation::Download,
        transfers: vec![knot_lfs::TransferAdapter::Basic],
        reference: None,
        objects: chunk
            .iter()
            .map(|(oid, size)| knot_lfs::BatchObject {
                oid: oid.clone(),
                size: *size,
            })
            .collect(),
        hash_algo: Some(knot_lfs::HashAlgo::Sha256),
    };
    let body = match serde_json::to_vec(&request_body) {
        Ok(body) => body,
        Err(_) => return all_missing(),
    };
    let mut request = knot_runtime::HttpRequest::post(batch_url.clone(), body.into());
    request.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(BATCH_MEDIA_TYPE),
    );
    request
        .headers
        .insert(header::ACCEPT, HeaderValue::from_static(BATCH_MEDIA_TYPE));
    let response = match state.git_http.execute(request).await {
        Ok(response) if response.status.is_success() => response,
        Ok(response) => {
            tracing::warn!(
                url = batch_url.as_str(),
                status = response.status.as_u16(),
                "upstream lfs batch refused"
            );
            return all_missing();
        }
        Err(fault) => {
            tracing::warn!(url = batch_url.as_str(), %fault, "upstream lfs batch failed");
            return all_missing();
        }
    };
    let parsed: BatchResponse = match serde_json::from_slice(&response.body) {
        Ok(parsed) => parsed,
        Err(fault) => {
            tracing::warn!(url = batch_url.as_str(), %fault, "upstream lfs batch unparsable");
            return all_missing();
        }
    };
    let declared: BTreeMap<LfsOid, ClaimedSize> = chunk.into_iter().collect();
    let unanswered = unanswered_oids(&declared, &parsed.objects);
    let tagged: Vec<(BatchResponseObject, ClaimedSize)> = parsed
        .objects
        .into_iter()
        .filter_map(|object| {
            declared
                .get(&object.oid)
                .copied()
                .map(|size| (object, size))
        })
        .collect();
    let failed: Vec<LfsOid> = futures::stream::iter(tagged)
        .then(|(object, size)| {
            fetch_remote_object(
                Arc::clone(&state),
                Arc::clone(&store),
                Arc::clone(&admission),
                fork.clone(),
                object,
                size,
            )
        })
        .filter_map(std::future::ready)
        .collect()
        .await;
    unanswered.into_iter().chain(failed).collect()
}

fn unanswered_oids(
    declared: &BTreeMap<LfsOid, ClaimedSize>,
    answered: &[BatchResponseObject],
) -> Vec<LfsOid> {
    let answered: BTreeSet<&LfsOid> = answered.iter().map(|object| &object.oid).collect();
    declared
        .keys()
        .filter(|oid| !answered.contains(oid))
        .cloned()
        .collect()
}

fn href_is_fetchable(url: &Url) -> bool {
    let scheme_ok = matches!(url.scheme(), "http" | "https");
    let host_ok = match url.host() {
        Some(url::Host::Ipv4(ip)) => !knot_runtime::is_blocked_ip(ip.into()),
        Some(url::Host::Ipv6(ip)) => !knot_runtime::is_blocked_ip(ip.into()),
        Some(url::Host::Domain(_)) => true,
        None => false,
    };
    scheme_ok && host_ok
}

async fn fetch_remote_object<H: HttpTransport, C: Clock>(
    state: Arc<XrpcState<H, C>>,
    store: Arc<knot_lfs::DiskStore>,
    admission: Arc<knot_lfs::StoreAdmission>,
    fork: RepoDid,
    object: BatchResponseObject,
    declared: ClaimedSize,
) -> Option<LfsOid> {
    let Some(action) = object.actions.and_then(|actions| actions.download) else {
        return Some(object.oid);
    };
    if !href_is_fetchable(&action.href) {
        tracing::warn!(
            oid = object.oid.as_str(),
            href = action.href.as_str(),
            "lfs fork download href isn't a public http target"
        );
        return Some(object.oid);
    }
    let permit = match admission.admit(declared) {
        Ok(permit) => permit,
        Err(fault) => {
            tracing::warn!(oid = object.oid.as_str(), %fault, "lfs fork download refused by admission");
            return Some(object.oid);
        }
    };
    let streamed = match state
        .git_http
        .execute_streamed(knot_runtime::HttpRequest::get(action.href))
        .await
    {
        Ok(streamed) if streamed.status.is_success() => streamed,
        _ => return Some(object.oid),
    };
    let landed = {
        use futures::TryStreamExt;
        let reader =
            tokio_util::io::StreamReader::new(streamed.body.map_err(std::io::Error::other));
        let oid = object.oid.clone();
        tokio::task::spawn_blocking(move || {
            let mut body = tokio_util::io::SyncIoBridge::new(reader);
            let outcome = store.put(&fork, &oid, declared, &mut body);
            drop(permit);
            outcome
        })
        .await
    };
    match landed {
        Ok(Ok(())) => None,
        Ok(Err(fault)) => {
            tracing::warn!(oid = object.oid.as_str(), %fault, "lfs fork download failed");
            Some(object.oid)
        }
        Err(_) => Some(object.oid),
    }
}

struct PermitBody {
    body: Body,
    _permit: OwnedSemaphorePermit,
}

impl http_body::Body for PermitBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        std::pin::Pin::new(&mut self.body).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_remote_batch_endpoint_matches_git_lfs_derivation() {
        let plain = Url::parse("https://nel.pet/did:web:witchcraft.systems/anemone").unwrap();
        assert_eq!(
            remote_batch_url(&plain).unwrap().as_str(),
            "https://nel.pet/did:web:witchcraft.systems/anemone.git/info/lfs/objects/batch"
        );
        let suffixed = Url::parse("https://nel.pet/did:plc:cuttle.git").unwrap();
        assert_eq!(
            remote_batch_url(&suffixed).unwrap().as_str(),
            "https://nel.pet/did:plc:cuttle.git/info/lfs/objects/batch"
        );
        let trailing = Url::parse("https://nel.pet/did:plc:cuttle/").unwrap();
        assert_eq!(
            remote_batch_url(&trailing).unwrap().as_str(),
            "https://nel.pet/did:plc:cuttle.git/info/lfs/objects/batch"
        );
        let decorated = Url::parse("https://nel.pet/did:plc:cuttle?ref=main#readme").unwrap();
        assert_eq!(
            remote_batch_url(&decorated).unwrap().as_str(),
            "https://nel.pet/did:plc:cuttle.git/info/lfs/objects/batch"
        );
    }

    #[test]
    fn oids_the_upstream_batch_never_answers_count_as_missing() {
        use sha2::{Digest, Sha256};
        let held = LfsOid::from_digest(Sha256::digest(b"held").into());
        let ignored = LfsOid::from_digest(Sha256::digest(b"ignored").into());
        let declared: BTreeMap<LfsOid, ClaimedSize> = [
            (held.clone(), ClaimedSize::new(4)),
            (ignored.clone(), ClaimedSize::new(7)),
        ]
        .into_iter()
        .collect();
        let answered = vec![BatchResponseObject {
            oid: held,
            size: ClaimedSize::new(4),
            authenticated: None,
            actions: None,
            error: None,
        }];
        assert_eq!(unanswered_oids(&declared, &answered), vec![ignored.clone()]);
        assert_eq!(
            unanswered_oids(&declared, &[]).len(),
            2,
            "an empty upstream response must leave every oid missing"
        );
    }

    #[test]
    fn a_stale_if_range_drops_the_range_for_a_full_response() {
        use axum::http::Request as HttpRequest;
        let etag = "\"6c17f2007cbe934aee6e309b28b2fba3c119d98be6ea4156da3aa3173456ad16\"";

        let matching = HttpRequest::builder()
            .header(header::IF_RANGE, etag)
            .header(header::RANGE, "bytes=0-9")
            .body(Body::empty())
            .unwrap();
        let kept = honor_if_range(matching, etag);
        assert!(kept.headers().get(header::IF_RANGE).is_none());
        assert!(
            kept.headers().get(header::RANGE).is_some(),
            "a matching validator keeps the range for a 206"
        );

        let stale = HttpRequest::builder()
            .header(header::IF_RANGE, "\"stale\"")
            .header(header::RANGE, "bytes=0-9")
            .body(Body::empty())
            .unwrap();
        let full = honor_if_range(stale, etag);
        assert!(full.headers().get(header::IF_RANGE).is_none());
        assert!(
            full.headers().get(header::RANGE).is_none(),
            "a stale validator drops the range so the client gets the whole object"
        );
    }

    #[test]
    fn revalidation_matches_the_oid_etag() {
        let etag = "\"6c17f2007cbe934aee6e309b28b2fba3c119d98be6ea4156da3aa3173456ad16\"";
        let holds =
            |value: &str| client_holds_current(Some(&HeaderValue::from_str(value).unwrap()), etag);
        assert!(holds(etag));
        assert!(holds(&format!("W/{etag}")));
        assert!(holds(&format!("\"other\", {etag}")));
        assert!(holds("*"));
        assert!(!holds("\"other\""));
        assert!(!client_holds_current(None, etag));
    }
}
