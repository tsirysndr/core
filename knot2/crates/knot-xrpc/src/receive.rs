use std::future::Future;
use std::io::Read;
use std::pin::Pin;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use futures::TryStreamExt;
use knot_messages::{ErrorKey, HttpMessages};
use knot_pack::{PackError, PackReceiver, ReceivedPack, SocketPeer};
use knot_receive::{Push, land};
use knot_runtime::{Clock, HttpTransport};
use knot_types::{AccountDid, ActorId, ObjectFormat, RepoDid};
use tokio_util::io::{StreamReader, SyncIoBridge};

use crate::{XrpcError, XrpcState, authenticate_and_authorize_push, run_blocking};

const READ_CHUNK: usize = 64 * 1024;
const ADVERTISEMENT: &str = "application/x-git-receive-pack-advertisement";
const RESULT: &str = "application/x-git-receive-pack-result";

pub(crate) fn routes<H: HttpTransport, C: Clock>() -> Router<Arc<XrpcState<H, C>>> {
    Router::new()
        .route(
            "/{did}/{name}/git-receive-pack",
            post(receive_named::<H, C>),
        )
        .route("/{did}/git-receive-pack", post(receive_did::<H, C>))
        .layer(DefaultBodyLimit::disable())
}

pub fn advertiser<H: HttpTransport, C: Clock>(
    state: Arc<XrpcState<H, C>>,
) -> Arc<dyn knot_pack::ReceiveAdvertiser> {
    Arc::new(ReceiveGate { state })
}

struct ReceiveGate<H, C> {
    state: Arc<XrpcState<H, C>>,
}

impl<H: HttpTransport, C: Clock> knot_pack::ReceiveAdvertiser for ReceiveGate<H, C> {
    fn advertise(
        &self,
        repo: RepoDid,
        peer: SocketPeer,
        headers: HeaderMap,
    ) -> Pin<Box<dyn Future<Output = Response> + Send + '_>> {
        Box::pin(async move { serve_advertisement(&self.state, repo, peer, &headers).await })
    }
}

async fn serve_advertisement<H: HttpTransport, C: Clock>(
    state: &Arc<XrpcState<H, C>>,
    repo: RepoDid,
    peer: SocketPeer,
    headers: &HeaderMap,
) -> Response {
    if let Err(response) = authorized_pusher(state, peer, headers, &repo).await {
        return response;
    }
    let layout = state.layout.clone();
    let target = repo.clone();
    let missing = state.catalog.http.repo_not_found.text();
    let advert = run_blocking(move || {
        let repo = layout
            .open(&target)
            .map_err(|_| XrpcError::not_found(missing))?;
        knot_pack::advertise_receive(&repo).map_err(map_pack)
    })
    .await;
    match advert {
        Ok(body) => git_response(ADVERTISEMENT, body),
        Err(error) => error.into_response(),
    }
}

async fn receive_named<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    Path(crate::RepoPathParams { did, name }): Path<crate::RepoPathParams>,
    peer: SocketPeer,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let repo = match crate::resolve_repo_named(&state, &did, &name).await {
        Ok(repo) => repo,
        Err(error) => return error.into_response(),
    };
    serve_receive(&state, repo, peer, &headers, body).await
}

async fn receive_did<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    Path(did): Path<crate::RepoDidSegment>,
    peer: SocketPeer,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let repo = match crate::resolve_repo_did(&state, &did) {
        Ok(repo) => repo,
        Err(error) => return error.into_response(),
    };
    serve_receive(&state, repo, peer, &headers, body).await
}

async fn serve_receive<H: HttpTransport, C: Clock>(
    state: &Arc<XrpcState<H, C>>,
    repo_did: RepoDid,
    peer: SocketPeer,
    headers: &HeaderMap,
    body: Body,
) -> Response {
    let pusher = match authorized_pusher(state, peer, headers, &repo_did).await {
        Ok(pusher) => pusher,
        Err(response) => return response,
    };

    let knot_actor = match state.secrets.public_key(&state.knot_did) {
        Ok(public) => ActorId::from_secp256k1(public.as_bytes()),
        Err(error) => return XrpcError::from(error).into_response(),
    };

    let format = {
        let layout = state.layout.clone();
        let target = repo_did.clone();
        let missing = state.catalog.http.repo_not_found.text();
        match run_blocking(move || {
            layout
                .open(&target)
                .map(|repo| repo.object_format())
                .map_err(|_| XrpcError::not_found(missing))
        })
        .await
        {
            Ok(format) => format,
            Err(error) => return error.into_response(),
        }
    };

    let _receive_permit = state.slots.receive.acquire().await;

    let received = {
        let scratch = state.layout.scratch_dir().to_path_buf();
        let limits = state.pack_limits;
        let limit = state.byte_limits.pack;
        let catalog = Arc::clone(&state.catalog);
        let reader = StreamReader::new(body.into_data_stream().map_err(std::io::Error::other));
        run_blocking(move || {
            drain_pack(
                SyncIoBridge::new(reader),
                &scratch,
                limit,
                limits,
                format,
                &catalog.http,
            )
        })
        .await
    };
    let received = match received {
        Ok(received) => received,
        Err(error) => return error.into_response(),
    };
    if received.is_empty() {
        return git_response(RESULT, Vec::new());
    }

    let landed = land(Push {
        layout: &state.layout,
        repo_did: &repo_did,
        received,
        limits: state.pack_limits,
        knot_actor,
        committer: pusher,
        events: Arc::clone(&state.events),
        index: &state.index,
        atproto: &state.atproto,
        resolve_slots: &state.slots.resolve,
        appview: &state.appview,
        maintenance: &state.maintenance,
        hostname: &state.knot_hostname,
        languages_push_budget: state.budgets.languages_push,
        catalog: Arc::clone(&state.catalog),
        ci_logs: state.ci_logs.clone(),
    })
    .await;
    match landed {
        Ok(framed) => git_response(RESULT, framed),
        Err(error) => {
            tracing::warn!(repo = repo_did.as_str(), %error, "http receive-pack failed");
            error.into_response()
        }
    }
}

fn drain_pack<R: Read>(
    mut reader: R,
    scratch: &std::path::Path,
    limit: knot_pack::MaxWireBytes,
    limits: knot_pack::PackLimits,
    format: ObjectFormat,
    messages: &HttpMessages,
) -> Result<ReceivedPack, XrpcError> {
    let mut receiver = PackReceiver::new(scratch, limit, limits, format.kind())
        .map_err(|error| XrpcError::internal(format!("receive staging failed: {error}")))?;
    let mut buffer = [0u8; READ_CHUNK];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| XrpcError::invalid_request(format!("receive read error: {error}")))?;
        if read == 0 {
            break;
        }
        if receiver
            .write(&buffer[..read])
            .map_err(|error| map_receive_read(error, messages))?
        {
            break;
        }
    }
    receiver
        .finish()
        .map_err(|error| map_receive_read(error, messages))
}

async fn authorized_pusher<H: HttpTransport, C: Clock>(
    state: &Arc<XrpcState<H, C>>,
    peer: SocketPeer,
    headers: &HeaderMap,
    repo: &RepoDid,
) -> Result<AccountDid, Response> {
    let denied = state.catalog.http.push_denied.text();
    authenticate_and_authorize_push(state, peer, headers, repo, &denied)
        .await
        .map_err(challenge)
}

fn challenge(error: XrpcError) -> Response {
    if error.status() == StatusCode::UNAUTHORIZED {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, crate::BASIC_CHALLENGE)],
            error.to_string(),
        )
            .into_response()
    } else {
        error.into_response()
    }
}

fn git_response(content_type: &'static str, body: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-cache, max-age=0, must-revalidate"),
            ),
        ],
        body,
    )
        .into_response()
}

fn map_pack(error: PackError) -> XrpcError {
    XrpcError::named(error.http_status(), "PackError", error.to_string())
}

fn map_receive_read(error: knot_pack::ReceiveReadError, messages: &HttpMessages) -> XrpcError {
    match error {
        knot_pack::ReceiveReadError::TooLarge => {
            XrpcError::request_too_large(messages.push_too_large.text())
        }
        knot_pack::ReceiveReadError::Truncated => {
            XrpcError::invalid_request(messages.receive_ended_early.text())
        }
        knot_pack::ReceiveReadError::Pack(error) => XrpcError::invalid_request(
            messages
                .malformed_pack
                .line(|ErrorKey::Error| error.to_string()),
        ),
        knot_pack::ReceiveReadError::Io(error) => {
            XrpcError::internal(format!("receive io error: {error}"))
        }
    }
}
