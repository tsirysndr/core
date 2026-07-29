mod archive;
mod cache;
mod error;
mod fetch;
mod frame;
mod guard;
mod ids;
mod idxwrite;
mod meter;
mod objects;
mod oids;
mod pkt;
mod quarantine;
mod receive;
mod receiver;
mod resolve;
mod upload;

use std::collections::HashMap;
use std::io::{self, Read};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, header};
use axum::response::Response;
use axum::routing::post;
use knot_git::{Layout, Repo};
use knot_messages::{Catalog, ErrorKey, FetchMessages};
use knot_resource::{PackSlots, SlotPermit};
use knot_runtime::Clock;
use knot_types::{
    AccountDid, ClonePath, Handle, KnotHostname, OwnerDid, OwnerRef, ParseError, RepoDid,
};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

pub use cache::{CacheConfig, MaxCacheBytes, MaxEntryBytes};
pub use error::{PackError, PackLimit};
pub use fetch::{FetchError, UpstreamRefs, local_pack, local_refs, remote_pack, remote_refs};
pub use frame::{
    ReceiveFramer, UploadFramer, archive_request_complete, receive_request_complete, upload_v0_nak,
};
pub use guard::PushGuard;
pub use ids::{DeltaDepth, MaxObjectBytes, MaxTotalBytes, MaxWireBytes};
pub use meter::PackLimits;
pub use objects::{ExpandedPack, count_expanded, write_expanded, write_pack};
pub use oids::{HaveOids, WantOids};
pub use pkt::frame_report;
pub use quarantine::sweep_incoming;
pub use receive::{Preflight, ReceiveCommand, ReceiveGuard, ReceiveOutcome, RefDecision};
pub use receiver::{PackReceiver, ReceiveReadError, ReceivedPack};
pub use upload::{SelectionLimits, init_selection_limits, selection_budget};

use upload::UploadOutcome;

pub use knot_messages::default_catalog;

pub fn default_hostname() -> &'static KnotHostname {
    static HOSTNAME: std::sync::LazyLock<KnotHostname> = std::sync::LazyLock::new(|| {
        KnotHostname::new("knot.invalid").expect("static hostname parses")
    });
    &HOSTNAME
}

pub fn advertise_upload(repo: &Repo) -> Result<Vec<u8>, PackError> {
    upload::advertise(repo)
}

pub fn advertise_upload_v0(repo: &Repo) -> Result<Vec<u8>, PackError> {
    upload::advertise_v0(repo)
}

pub fn upload_pack(repo: &Repo, request: &[u8]) -> Result<Vec<u8>, PackError> {
    upload::buffered(repo, request, &default_catalog().fetch, default_hostname())
}

pub fn upload_pack_streamed(
    repo: &Repo,
    request: &[u8],
    messages: &FetchMessages,
    knot: &KnotHostname,
    sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> Result<(), PackError> {
    upload::streamed(repo, request, messages, knot, sink)
}

pub fn advertise_receive(repo: &Repo) -> Result<Vec<u8>, PackError> {
    receive::advertise(repo)
}

pub fn advertise_upload_ssh(repo: &Repo) -> Result<Vec<u8>, PackError> {
    upload::advertise_ssh(repo)
}

pub fn advertise_upload_v0_ssh(repo: &Repo) -> Result<Vec<u8>, PackError> {
    upload::advertise_v0_ssh(repo)
}

pub fn advertise_receive_ssh(repo: &Repo) -> Result<Vec<u8>, PackError> {
    receive::advertise_ssh(repo)
}

#[doc(hidden)]
pub fn receive_pack(repo: &Repo, request: &[u8]) -> Result<Vec<u8>, PackError> {
    receive::handle_bytes(repo, request, &PackLimits::default())
}

#[doc(hidden)]
pub fn receive_pack_with_limits(
    repo: &Repo,
    request: &[u8],
    limits: &PackLimits,
) -> Result<Vec<u8>, PackError> {
    receive::handle_bytes(repo, request, limits)
}

#[doc(hidden)]
pub fn bench_ingest_fresh(
    objects_dir: &std::path::Path,
    pack_path: &std::path::Path,
    kind: gix::hash::Kind,
) -> Result<bool, PackError> {
    let pack = gix_pack::data::File::at(pack_path, kind)
        .map_err(|error| PackError::Pack(error.to_string()))?;
    objects::ingest_and_close(
        objects_dir,
        &pack,
        &PackLimits::default(),
        kind,
        knot_resource::ingest_base_budget(),
        false,
    )
    .map(|closure| closure.is_some())
}

#[doc(hidden)]
pub fn bench_ingest_external(
    objects_dir: &std::path::Path,
    pack_path: &std::path::Path,
    kind: gix::hash::Kind,
) -> Result<Option<bool>, PackError> {
    let pack = gix_pack::data::File::at(pack_path, kind)
        .map_err(|error| PackError::Pack(error.to_string()))?;
    objects::admit_ingest(&pack, kind)?;
    objects::ingest_and_close(
        objects_dir,
        &pack,
        &PackLimits::default(),
        kind,
        knot_resource::ingest_base_budget(),
        true,
    )
    .map(|closure| closure.map(|closure| closure.self_contained))
}

#[doc(hidden)]
pub fn bench_admit_and_ingest(
    objects_dir: &std::path::Path,
    pack_path: &std::path::Path,
    kind: gix::hash::Kind,
) -> Result<bool, PackError> {
    bench_ingest_with_base_budget(
        objects_dir,
        pack_path,
        kind,
        knot_resource::ingest_base_budget(),
    )
}

#[doc(hidden)]
pub fn bench_ingest_with_base_budget(
    objects_dir: &std::path::Path,
    pack_path: &std::path::Path,
    kind: gix::hash::Kind,
    base_budget: Option<usize>,
) -> Result<bool, PackError> {
    let pack = gix_pack::data::File::at(pack_path, kind)
        .map_err(|error| PackError::Pack(error.to_string()))?;
    objects::admit_ingest(&pack, kind)?;
    objects::ingest_and_close(
        objects_dir,
        &pack,
        &PackLimits::default(),
        kind,
        base_budget,
        false,
    )
    .map(|closure| closure.is_some())
}

pub fn receive_pack_guarded(
    repo: &Repo,
    request: &[u8],
    limits: &PackLimits,
    guard: &dyn ReceiveGuard,
    seal: &dyn Fn(&[knot_git::RefUpdate]),
    messages: &knot_messages::RejectMessages,
) -> Result<ReceiveOutcome, PackError> {
    receive::handle_guarded_bytes(repo, request, limits, guard, seal, messages)
}

pub fn receive_pack_guarded_streamed(
    repo: &Repo,
    received: &ReceivedPack,
    limits: &PackLimits,
    guard: &dyn ReceiveGuard,
    seal: &dyn Fn(&[knot_git::RefUpdate]),
    messages: &knot_messages::RejectMessages,
) -> Result<ReceiveOutcome, PackError> {
    receive::handle_guarded_streamed(repo, received, limits, guard, seal, messages)
}

pub fn receive_preflight(request: &[u8]) -> Preflight {
    receive::preflight(request)
}

pub fn upload_archive_streamed(
    repo: &Repo,
    request: &[u8],
    limit: knot_git::ArchiveLimit,
    sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> Result<(), PackError> {
    archive::stream(repo, request, limit, sink)
}

pub fn upload_archive(
    repo: &Repo,
    request: &[u8],
    limit: knot_git::ArchiveLimit,
) -> Result<Vec<u8>, PackError> {
    let mut buf = Vec::new();
    upload_archive_streamed(repo, request, limit, &mut |chunk| {
        buf.extend_from_slice(chunk);
        Ok(())
    })?;
    Ok(buf)
}

pub fn meter_pack(
    pack: &[u8],
    limits: &PackLimits,
    kind: gix::hash::Kind,
) -> Result<(), PackError> {
    meter::meter(pack, limits, kind)
}

pub fn ingest_pack(
    objects_dir: &std::path::Path,
    pack: &[u8],
    limits: &PackLimits,
    kind: gix::hash::Kind,
) -> Result<(), PackError> {
    objects::index_pack(objects_dir, pack, limits, kind)
}

#[doc(hidden)]
pub mod fuzz {
    pub fn pkt(data: &[u8]) {
        let _ = crate::pkt::data_payloads(data);
        let _ = crate::pkt::data_payloads_all(data);
        let _ = crate::pkt::split_receive(data);
    }

    pub fn pack(data: &[u8]) {
        let _ = crate::meter::meter(data, &crate::PackLimits::default(), gix::hash::Kind::Sha1);
    }

    pub fn receive_commands(data: &[u8]) {
        crate::receive::fuzz(data);
    }

    pub fn upload_args(data: &[u8]) {
        crate::upload::fuzz(data);
    }
}

const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoTarget {
    Did(RepoDid),
    OwnerPath(OwnerDid, ClonePath),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoLookup {
    Hosted(RepoDid),
    Unhosted,
    Unavailable,
}

impl RepoLookup {
    pub fn from_resolved<T>(
        resolved: knot_index::Resolved<Option<T>>,
        found: impl FnOnce(T) -> RepoDid,
    ) -> RepoLookup {
        match resolved {
            knot_index::Resolved::Ready(Some(value)) => RepoLookup::Hosted(found(value)),
            knot_index::Resolved::Ready(None) => RepoLookup::Unhosted,
            knot_index::Resolved::Warming => RepoLookup::Unavailable,
        }
    }
}

pub trait RepoResolver: Send + Sync + 'static {
    fn resolve(&self, target: &RepoTarget) -> RepoLookup;
}

pub trait HandleResolver: Send + Sync + 'static {
    fn resolve(
        &self,
        handle: Handle,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<AccountDid>> + Send + '_>>;
}

pub use knot_edge::SocketPeer;

pub trait ReceiveAdvertiser: Send + Sync + 'static {
    fn advertise(
        &self,
        repo: RepoDid,
        peer: knot_edge::SocketPeer,
        headers: HeaderMap,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send + '_>>;
}

impl<F> RepoResolver for F
where
    F: Fn(&RepoTarget) -> RepoLookup + Send + Sync + 'static,
{
    fn resolve(&self, target: &RepoTarget) -> RepoLookup {
        self(target)
    }
}

#[derive(Clone)]
struct PackState {
    layout: Layout,
    resolver: Arc<dyn RepoResolver>,
    receive: Option<Arc<dyn ReceiveAdvertiser>>,
    handle_resolver: Option<Arc<dyn HandleResolver>>,
    pack_slots: PackSlots,
    cache: Arc<cache::PackCache>,
    catalog: Arc<Catalog>,
    hostname: KnotHostname,
    archive_limit: knot_git::ArchiveLimit,
}

pub struct EdgeConfig {
    pub layout: Layout,
    pub resolver: Arc<dyn RepoResolver>,
    pub receive: Option<Arc<dyn ReceiveAdvertiser>>,
    pub handle_resolver: Option<Arc<dyn HandleResolver>>,
    pub pack_slots: PackSlots,
    pub cache: CacheConfig,
    pub catalog: Arc<Catalog>,
    pub hostname: KnotHostname,
    pub clock: Arc<dyn Clock>,
    pub archive_limit: knot_git::ArchiveLimit,
}

impl EdgeConfig {
    pub fn serving(layout: Layout, resolver: Arc<dyn RepoResolver>, clock: Arc<dyn Clock>) -> Self {
        Self {
            layout,
            resolver,
            receive: None,
            handle_resolver: None,
            pack_slots: PackSlots::new(knot_resource::threads().get()),
            cache: CacheConfig::default(),
            catalog: Arc::new(Catalog::defaults()),
            hostname: default_hostname().clone(),
            clock,
            archive_limit: knot_git::ArchiveLimit::default(),
        }
    }

    pub fn with_pack_slots(self, pack_slots: PackSlots) -> Self {
        Self { pack_slots, ..self }
    }
}

pub fn router(layout: Layout, resolver: Arc<dyn RepoResolver>, clock: Arc<dyn Clock>) -> Router {
    serving_router(EdgeConfig::serving(layout, resolver, clock))
}

pub fn router_with_pack_slots(
    layout: Layout,
    resolver: Arc<dyn RepoResolver>,
    pack_slots: PackSlots,
    clock: Arc<dyn Clock>,
) -> Router {
    serving_router(EdgeConfig::serving(layout, resolver, clock).with_pack_slots(pack_slots))
}

fn serving_router(config: EdgeConfig) -> Router {
    let state = pack_state(config);
    write_routes(state.clone()).merge(advertisement_routes(state).into_router())
}

pub fn edge_routes(config: EdgeConfig) -> (Router, knot_edge::ZeroRttRoutes) {
    let state = pack_state(config);
    (write_routes(state.clone()), advertisement_routes(state))
}

fn pack_state(config: EdgeConfig) -> PackState {
    let EdgeConfig {
        layout,
        resolver,
        receive,
        handle_resolver,
        pack_slots,
        cache,
        catalog,
        hostname,
        clock,
        archive_limit,
    } = config;
    PackState {
        layout,
        resolver,
        receive,
        handle_resolver,
        pack_slots,
        cache: cache::PackCache::new(cache, clock),
        catalog,
        hostname,
        archive_limit,
    }
}

fn write_routes(state: PackState) -> Router {
    Router::new()
        .route("/{did}/{name}/git-upload-pack", post(upload_named))
        .route(
            "/{did}/{name}/git-upload-archive",
            post(upload_archive_named),
        )
        .route("/{did}/git-upload-pack", post(upload_did))
        .route("/{did}/git-upload-archive", post(upload_archive_did))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(state)
}

fn advertisement_routes(state: PackState) -> knot_edge::ZeroRttRoutes {
    let named_state = state.clone();
    let did_state = state;
    knot_edge::ZeroRttRoutes::new()
        .get(
            "/{did}/{name}/info/refs",
            knot_edge::ZeroRttSafe::new(
                move |Path((did, name)): Path<(String, String)>,
                      Query(query): Query<HashMap<String, String>>,
                      peer: knot_edge::SocketPeer,
                      headers: HeaderMap| {
                    let state = named_state.clone();
                    async move {
                        let owner = resolve_owner(&state, &did).await?;
                        let repo_did = resolve_named_did(&state, &owner, &name)?;
                        read_advertisement(
                            &state,
                            repo_did,
                            query.get("service").map(String::as_str),
                            peer,
                            &headers,
                        )
                        .await
                    }
                },
            ),
        )
        .get(
            "/{did}/info/refs",
            knot_edge::ZeroRttSafe::new(
                move |Path(did): Path<String>,
                      Query(query): Query<HashMap<String, String>>,
                      peer: knot_edge::SocketPeer,
                      headers: HeaderMap| {
                    let state = did_state.clone();
                    async move {
                        let repo_did = resolve_did_did(&state, &did)?;
                        read_advertisement(
                            &state,
                            repo_did,
                            query.get("service").map(String::as_str),
                            peer,
                            &headers,
                        )
                        .await
                    }
                },
            ),
        )
}

fn lookup_did(lookup: RepoLookup) -> Result<RepoDid, PackError> {
    match lookup {
        RepoLookup::Hosted(did) => Ok(did),
        RepoLookup::Unhosted => Err(PackError::NotFound),
        RepoLookup::Unavailable => Err(PackError::Unavailable),
    }
}

fn bad_path(error: ParseError) -> PackError {
    PackError::BadPath(error.to_string())
}

async fn resolve_owner(state: &PackState, owner: &str) -> Result<OwnerDid, PackError> {
    match OwnerRef::parse(owner).ok_or(PackError::NotFound)? {
        OwnerRef::Did(did) => Ok(did),
        OwnerRef::Handle(handle) => {
            let resolver = state.handle_resolver.as_ref().ok_or(PackError::NotFound)?;
            let did = resolver.resolve(handle).await.ok_or(PackError::NotFound)?;
            Ok(did.into())
        }
    }
}

fn resolve_named_did(
    state: &PackState,
    owner: &OwnerDid,
    name: &str,
) -> Result<RepoDid, PackError> {
    let path = ClonePath::parse(name).ok_or(PackError::NotFound)?;
    lookup_did(
        state
            .resolver
            .resolve(&RepoTarget::OwnerPath(owner.clone(), path)),
    )
}

fn resolve_did_did(state: &PackState, did: &str) -> Result<RepoDid, PackError> {
    let did = RepoDid::new(did).map_err(bad_path)?;
    lookup_did(state.resolver.resolve(&RepoTarget::Did(did)))
}

fn open_named(state: &PackState, owner: &OwnerDid, name: &str) -> Result<Repo, PackError> {
    let did = resolve_named_did(state, owner, name)?;
    state.layout.open(&did).map_err(|_| PackError::NotFound)
}

fn open_did(state: &PackState, did: &str) -> Result<Repo, PackError> {
    let did = resolve_did_did(state, did)?;
    state.layout.open(&did).map_err(|_| PackError::NotFound)
}

async fn read_advertisement(
    state: &PackState,
    repo_did: RepoDid,
    service: Option<&str>,
    peer: knot_edge::SocketPeer,
    headers: &HeaderMap,
) -> Result<Response, PackError> {
    match service {
        Some("git-upload-pack") => {
            let repo = state
                .layout
                .open(&repo_did)
                .map_err(|_| PackError::NotFound)?;
            let body = if wants_v2(headers) {
                advertise_upload(&repo)?
            } else {
                upload::advertise_v0(&repo)?
            };
            Ok(git_response(
                "application/x-git-upload-pack-advertisement",
                body,
            ))
        }
        Some("git-receive-pack") => match &state.receive {
            Some(advertiser) => Ok(advertiser.advertise(repo_did, peer, headers.clone()).await),
            None => Err(PackError::PushOverSsh),
        },
        _ => Err(PackError::UnsupportedService),
    }
}

fn decode_request(headers: &HeaderMap, body: Bytes) -> Result<Vec<u8>, PackError> {
    let codings: Vec<&str> = headers
        .get(header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|token| !token.is_empty() && !token.eq_ignore_ascii_case("identity"))
                .collect()
        })
        .unwrap_or_default();
    match codings.as_slice() {
        [] => Ok(body.to_vec()),
        [token] if token.eq_ignore_ascii_case("gzip") => {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(body.as_ref())
                .take(MAX_REQUEST_BYTES as u64 + 1)
                .read_to_end(&mut out)
                .map_err(|error| PackError::Protocol(format!("gzip request body: {error}")))?;
            match out.len() > MAX_REQUEST_BYTES {
                true => Err(PackError::LimitExceeded(PackLimit::TotalBytes)),
                false => Ok(out),
            }
        }
        unsupported => Err(PackError::UnsupportedEncoding(unsupported.join(", "))),
    }
}

fn wants_v2(headers: &HeaderMap) -> bool {
    headers
        .get("git-protocol")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(':').any(|token| token.trim() == "version=2"))
}

fn refs_digest(repo: &Repo) -> Option<ids::RefsDigest> {
    let refs = repo
        .advertised_refs_for(knot_git::AdvertScope::Upload)
        .ok()?;
    let mut hasher = gix::hash::hasher(gix::hash::Kind::Sha256);
    refs.iter().for_each(|record| {
        hasher.update(record.name.as_str().as_bytes());
        hasher.update(b"\0");
        hasher.update(record.target.to_string().as_bytes());
        hasher.update(b"\n");
    });
    let id = hasher.try_finalize().ok()?;
    let mut token = [0u8; 32];
    token.copy_from_slice(id.as_slice());
    Some(ids::RefsDigest::new(token))
}

const CACHE_RETRY_BUDGET: usize = 4;

async fn upload_dispatch(
    state: &PackState,
    repo: Repo,
    body: &[u8],
) -> Result<Response, PackError> {
    upload_dispatch_within(state, repo, body, CACHE_RETRY_BUDGET).await
}

async fn upload_dispatch_within(
    state: &PackState,
    repo: Repo,
    body: &[u8],
    retries: usize,
) -> Result<Response, PackError> {
    let Some(token) = refs_digest(&repo) else {
        return dispatch_plan(state, repo, body, None).await;
    };
    let key = cache::RequestKey::new(&repo.objects_dir(), &token, body);
    match state.cache.decide(key) {
        cache::Decision::Serve(bytes) => Ok(cached_response(bytes)),
        cache::Decision::Await(receiver) => match cache::wait(receiver).await {
            cache::Resolved::Bytes(bytes) => Ok(cached_response(bytes)),
            cache::Resolved::Retry => match retries {
                0 => dispatch_plan(state, repo, body, None).await,
                _ => Box::pin(upload_dispatch_within(state, repo, body, retries - 1)).await,
            },
            cache::Resolved::Regenerate => dispatch_plan(state, repo, body, None).await,
        },
        cache::Decision::Lead(lease) => dispatch_plan(state, repo, body, Some(lease)).await,
        cache::Decision::Stream | cache::Decision::Off => {
            dispatch_plan(state, repo, body, None).await
        }
    }
}

async fn dispatch_plan(
    state: &PackState,
    repo: Repo,
    body: &[u8],
    lease: Option<cache::Lease>,
) -> Result<Response, PackError> {
    let permit = state.pack_slots.acquire().await;
    let owned_body = body.to_vec();
    let planned = tokio::task::spawn_blocking(move || {
        let outcome = upload::plan(&repo, &owned_body);
        (repo, outcome)
    })
    .await;
    let (repo, plan) = match planned {
        Ok((repo, Ok(plan))) => (repo, plan),
        Ok((_repo, Err(error))) => {
            if let Some(lease) = lease {
                lease.regenerate();
            }
            return Err(error);
        }
        Err(_join) => {
            if let Some(lease) = lease {
                lease.regenerate();
            }
            return Err(PackError::Pack("upload planning task panicked".to_string()));
        }
    };
    match plan {
        UploadOutcome::Buffered(bytes) => {
            drop(permit);
            if let Some(lease) = lease {
                lease.regenerate();
            }
            Ok(git_response("application/x-git-upload-pack-result", bytes))
        }
        UploadOutcome::Streaming {
            preamble,
            wants,
            haves,
            opts,
        } => Ok(stream_response(
            repo,
            preamble,
            wants,
            haves,
            opts,
            permit,
            lease,
            Arc::clone(&state.catalog),
            state.hostname.clone(),
        )),
    }
}

fn cached_response(bytes: Bytes) -> Response {
    nocache(
        Response::builder().header(header::CONTENT_TYPE, "application/x-git-upload-pack-result"),
    )
    .body(Body::from(bytes))
    .expect("valid response")
}

#[allow(clippy::too_many_arguments)]
fn stream_response(
    repo: Repo,
    preamble: Vec<u8>,
    wants: WantOids,
    haves: HaveOids,
    opts: upload::StreamOpts,
    permit: SlotPermit,
    lease: Option<cache::Lease>,
    catalog: Arc<Catalog>,
    hostname: KnotHostname,
) -> Response {
    let side_band = opts.side_band;
    let limit = lease.as_ref().map(cache::Lease::max_entry_bytes);
    let (tx, rx) = mpsc::channel::<Result<Bytes, io::Error>>(16);
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let capture = std::cell::RefCell::new(cache::Capture::new(limit));
        let mut emit = |chunk: &[u8]| -> io::Result<()> {
            capture.borrow_mut().record(chunk);
            tx.blocking_send(Ok(Bytes::copy_from_slice(chunk)))
                .map_err(|_| io::Error::other("client disconnected"))
        };
        if emit(&preamble).is_err() {
            if let Some(lease) = lease {
                lease.retry();
            }
            return;
        }
        let result = upload::stream_pack(
            &repo,
            &wants,
            &haves,
            &opts,
            &catalog.fetch,
            &hostname,
            &mut emit,
        );
        match result {
            Ok(()) => {
                if side_band {
                    let mut flush = Vec::new();
                    if pkt::write_flush(&mut flush).is_ok() {
                        let _ = emit(&flush);
                    }
                }
                if let Some(lease) = lease {
                    match capture.into_inner().into_bytes() {
                        Some(bytes) => lease.ready(Bytes::from(bytes)),
                        None => lease.too_large(),
                    }
                }
            }
            Err(error) if side_band => {
                let mut tail = Vec::new();
                let line = catalog
                    .fetch
                    .fatal
                    .line(|ErrorKey::Error| error.to_string().replace('\n', " "));
                let message = format!("{line}\n");
                if pkt::write_band_error(&mut tail, message.as_bytes()).is_ok() {
                    let _ = pkt::write_flush(&mut tail);
                    let _ = emit(&tail);
                }
                if let Some(lease) = lease {
                    lease.regenerate();
                }
            }
            Err(error) => {
                let _ = tx.blocking_send(Err(io::Error::other(error.to_string())));
                if let Some(lease) = lease {
                    lease.regenerate();
                }
            }
        }
    });
    nocache(
        Response::builder().header(header::CONTENT_TYPE, "application/x-git-upload-pack-result"),
    )
    .body(Body::from_stream(ReceiverStream::new(rx)))
    .expect("valid response")
}

async fn upload_named(
    State(state): State<PackState>,
    Path((did, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PackError> {
    let owner = resolve_owner(&state, &did).await?;
    let repo = open_named(&state, &owner, &name)?;
    upload_dispatch(&state, repo, &decode_request(&headers, body)?).await
}

async fn upload_did(
    State(state): State<PackState>,
    Path(did): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PackError> {
    let repo = open_did(&state, &did)?;
    upload_dispatch(&state, repo, &decode_request(&headers, body)?).await
}

async fn archive_dispatch(
    state: &PackState,
    repo: Repo,
    body: Vec<u8>,
) -> Result<Response, PackError> {
    let permit = state.pack_slots.acquire().await;
    Ok(archive_response(repo, body, state.archive_limit, permit))
}

fn archive_response(
    repo: Repo,
    body: Vec<u8>,
    limit: knot_git::ArchiveLimit,
    permit: SlotPermit,
) -> Response {
    let (tx, rx) = mpsc::channel::<Result<Bytes, io::Error>>(16);
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut sink = |chunk: &[u8]| -> io::Result<()> {
            tx.blocking_send(Ok(Bytes::copy_from_slice(chunk)))
                .map_err(|_| io::Error::other("client disconnected"))
        };
        if let Err(error) = upload_archive_streamed(&repo, &body, limit, &mut sink) {
            let _ = tx.blocking_send(Err(io::Error::other(error.to_string())));
        }
    });
    nocache(Response::builder().header(
        header::CONTENT_TYPE,
        "application/x-git-upload-archive-result",
    ))
    .body(Body::from_stream(ReceiverStream::new(rx)))
    .expect("valid response")
}

async fn upload_archive_named(
    State(state): State<PackState>,
    Path((did, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PackError> {
    let owner = resolve_owner(&state, &did).await?;
    let repo = open_named(&state, &owner, &name)?;
    archive_dispatch(&state, repo, decode_request(&headers, body)?).await
}

async fn upload_archive_did(
    State(state): State<PackState>,
    Path(did): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PackError> {
    let repo = open_did(&state, &did)?;
    archive_dispatch(&state, repo, decode_request(&headers, body)?).await
}

fn nocache(builder: axum::http::response::Builder) -> axum::http::response::Builder {
    builder
        .header(header::EXPIRES, "Fri, 01 Jan 1980 00:00:00 GMT")
        .header(header::PRAGMA, "no-cache")
        .header(
            header::CACHE_CONTROL,
            "no-cache, max-age=0, must-revalidate",
        )
}

fn git_response(content_type: &'static str, body: Vec<u8>) -> Response {
    nocache(Response::builder().header(header::CONTENT_TYPE, content_type))
        .body(Body::from(body))
        .expect("valid response")
}
