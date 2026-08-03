use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt;
use knot_acl::{KnotAcl, can_push};
use knot_index::Resolved;
use knot_lfs::TransferOp;
use knot_pack::{PackError, PackLimits, RepoLookup};
use knot_resource::SubjectKey;
use knot_runtime::{Clock, HttpTransport};
use knot_types::{AccountDid, ClonePath, ObjectFormat, OwnerDid, RepoDid};
use russh::Channel;
use russh::server::Msg;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::SshState;
use crate::identity::Credential;

const READ_CHUNK: usize = 64 * 1024;
const MAX_UPLOAD_REQUEST: usize = 16 * 1024 * 1024;
const RECEIVE_BODY_DEADLINE: Duration = Duration::from_secs(1800);
const ARCHIVE_REQUEST_DEADLINE: Duration = Duration::from_secs(60);
const LFS_PROGRESS_GRACE: Duration = Duration::from_secs(60);
const LFS_PROGRESS_FLOOR_BYTES_PER_SEC: u64 = 1024;
const LFS_STALL_TIMEOUT: Duration = Duration::from_secs(120);
const CANDIDATE_FANOUT: usize = 4;
const AUTHORIZED_NAMES_SHOWN: usize = 4;

fn lfs_within_progress_budget(waited: Duration, moved_bytes: u64) -> bool {
    waited
        <= LFS_PROGRESS_GRACE + Duration::from_secs(moved_bytes / LFS_PROGRESS_FLOOR_BYTES_PER_SEC)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Service {
    Upload,
    UploadArchive,
    Receive,
    Lfs(TransferOp),
}

enum ReadError {
    Io(std::io::Error),
    Pack(PackError),
    TooLarge,
    Truncated,
    Deadline,
}

enum RepoRef {
    Did(RepoDid),
    OwnerPath(OwnerDid, ClonePath),
    HandlePath(knot_types::Handle, ClonePath),
}

enum ResolvedRef {
    Did(RepoDid),
    OwnerPath(OwnerDid, ClonePath),
}

fn parse_exec(command: &[u8]) -> Option<(Service, RepoRef)> {
    let text = std::str::from_utf8(command).ok()?.trim();
    if let Some(rest) = text.strip_prefix("git-lfs-transfer ") {
        let (path, op_token) = rest.trim().rsplit_once(' ')?;
        let op = TransferOp::parse(op_token.trim())?;
        return Some((Service::Lfs(op), parse_repo_path(path)?));
    }
    let (service, rest) = [
        ("git-upload-pack ", Service::Upload),
        ("git upload-pack ", Service::Upload),
        ("git-upload-archive ", Service::UploadArchive),
        ("git upload-archive ", Service::UploadArchive),
        ("git-receive-pack ", Service::Receive),
        ("git receive-pack ", Service::Receive),
    ]
    .into_iter()
    .find_map(|(prefix, service)| text.strip_prefix(prefix).map(|rest| (service, rest)))?;
    Some((service, parse_repo_path(rest)?))
}

fn parse_repo_path(raw: &str) -> Option<RepoRef> {
    let path = raw
        .trim()
        .trim_matches('\'')
        .trim_matches('"')
        .trim_start_matches('/');
    match path.split_once('/') {
        Some((owner, name)) => {
            let candidates = ClonePath::parse(name)?;
            match knot_types::OwnerRef::parse(owner)? {
                knot_types::OwnerRef::Did(owner) => Some(RepoRef::OwnerPath(owner, candidates)),
                knot_types::OwnerRef::Handle(handle) => {
                    Some(RepoRef::HandlePath(handle, candidates))
                }
            }
        }
        None => Some(RepoRef::Did(RepoDid::new(path).ok()?)),
    }
}

fn resolve_repo_ref<H: HttpTransport, C: Clock>(
    state: &Arc<SshState<H, C>>,
    repo_ref: ResolvedRef,
) -> RepoLookup {
    let candidate = match repo_ref {
        ResolvedRef::Did(did) => RepoLookup::Hosted(did),
        ResolvedRef::OwnerPath(owner, candidates) => RepoLookup::from_resolved(
            state.index.resolve_clone_path(&owner, &candidates),
            |found| found,
        ),
    };
    match candidate {
        RepoLookup::Hosted(did) => {
            RepoLookup::from_resolved(state.index.owner_of(&did), |_| did.clone())
        }
        undecided => undecided,
    }
}

pub(crate) async fn run_exec<H: HttpTransport, C: Clock>(
    state: Arc<SshState<H, C>>,
    credential: Credential,
    channel: Channel<Msg>,
    command: &[u8],
    protocol_v2: bool,
    peer: Option<IpAddr>,
) {
    let Some((service, repo_ref)) = parse_exec(command) else {
        fail(channel, &state.catalog.ssh.unsupported_command.text()).await;
        return;
    };
    let peer_limiter = match &service {
        Service::Lfs(_) => state
            .lfs
            .as_ref()
            .map_or(&state.peer_slots, |lfs| &lfs.peer_slots),
        _ => &state.peer_slots,
    };
    let _peer_guard = match peer_limiter.admit(peer, state.atproto.now()) {
        Ok(guard) => guard,
        Err(refusal) => {
            let reason = match refusal {
                knot_resource::Refusal::RateLimited => "peer request rate exceeded",
                knot_resource::Refusal::Saturated => "peer concurrency limit reached",
            };
            tracing::warn!(?peer, reason, "ssh exec rejected");
            return fail(channel, &state.catalog.ssh.too_many_operations.text()).await;
        }
    };
    let resolved_ref = match repo_ref {
        RepoRef::Did(did) => ResolvedRef::Did(did),
        RepoRef::OwnerPath(owner, candidates) => ResolvedRef::OwnerPath(owner, candidates),
        RepoRef::HandlePath(owner_handle, candidates) => {
            let Some(_lookup_permit) = state.lookup_slots.try_acquire() else {
                tracing::warn!(
                    ?peer,
                    "ssh exec rejected, the lookup budget can't resolve another handle"
                );
                return fail(channel, &state.catalog.ssh.too_many_operations.text()).await;
            };
            match state
                .atproto
                .resolve_handle_to_did(&owner_handle)
                .await
                .ok()
            {
                Some(did) => ResolvedRef::OwnerPath(did.into(), candidates),
                None => {
                    fail(channel, &state.catalog.ssh.repo_not_found.text()).await;
                    return;
                }
            }
        }
    };
    let repo_did = match resolve_repo_ref(&state, resolved_ref) {
        RepoLookup::Hosted(did) => did,
        RepoLookup::Unhosted => {
            fail(channel, &state.catalog.ssh.repo_not_found.text()).await;
            return;
        }
        RepoLookup::Unavailable => {
            fail(channel, &state.catalog.ssh.index_warming.text()).await;
            return;
        }
    };
    let layout = state.layout.clone();
    let did = repo_did.clone();
    let opened = tokio::task::spawn_blocking(move || layout.open(&did).is_ok())
        .await
        .unwrap_or(false);
    if !opened {
        fail(channel, &state.catalog.ssh.repo_not_found.text()).await;
        return;
    }
    match service {
        Service::Upload => serve_upload(state, channel, repo_did, protocol_v2).await,
        Service::UploadArchive => serve_upload_archive(state, channel, repo_did).await,
        Service::Receive => serve_receive(state, credential, channel, repo_did, peer).await,
        Service::Lfs(op) => serve_lfs(state, credential, channel, repo_did, op, peer).await,
    }
}

async fn serve_lfs<H: HttpTransport, C: Clock>(
    state: Arc<SshState<H, C>>,
    credential: Credential,
    mut channel: Channel<Msg>,
    repo_did: RepoDid,
    op: TransferOp,
    peer: Option<IpAddr>,
) {
    let Some(lfs) = state.lfs.clone() else {
        return fail(channel, &state.catalog.ssh.lfs_disabled.text()).await;
    };
    if op == TransferOp::Upload
        && let PushAuth::Refused { reason, message } =
            authorize_push(&state, &credential, &repo_did, peer).await
    {
        tracing::warn!(repo = repo_did.as_str(), reason, "ssh lfs upload denied");
        return fail(channel, &message).await;
    }
    let permit = match Arc::clone(&lfs.slots).acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => return fail(channel, &state.catalog.ssh.shutting_down.text()).await,
    };
    let started = std::time::Instant::now();

    let (tx, rx) = mpsc::channel::<Vec<u8>>(8);
    let writer = Box::pin(channel.make_writer());
    let handle = lfs.handle.clone();
    let runtime = Handle::current();
    let did = repo_did.clone();
    let catalog = Arc::clone(&state.catalog);
    let mut engine = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let output = std::io::BufWriter::new(MeteredWrite::new(runtime.clone(), writer));
        knot_lfs::serve_transfer(
            handle.store.as_ref(),
            handle.admission.as_ref(),
            &did,
            op,
            &catalog.lfs,
            MpscRead::new(runtime, rx),
            output,
        )
    });
    let joined = {
        let reader = channel.make_reader();
        tokio::select! {
            joined = &mut engine => joined,
            () = pump_input(reader, tx) => engine.await,
        }
    };
    let status = match joined {
        Ok(Ok(())) => {
            tracing::info!(
                repo = repo_did.as_str(),
                op = match op {
                    TransferOp::Upload => "upload",
                    TransferOp::Download => "download",
                },
                duration_ms = started.elapsed().as_millis() as u64,
                "ssh lfs transfer finished"
            );
            0
        }
        Ok(Err(fault)) => {
            tracing::warn!(repo = repo_did.as_str(), %fault, "ssh lfs transfer failed");
            1
        }
        Err(join) => {
            tracing::error!(repo = repo_did.as_str(), %join, "ssh lfs transfer task panicked");
            1
        }
    };
    finish(channel, status).await;
}

async fn pump_input<R: AsyncRead + Unpin>(reader: R, tx: mpsc::Sender<Vec<u8>>) {
    use futures::TryStreamExt;
    let _ = tokio_util::io::ReaderStream::with_capacity(reader, READ_CHUNK)
        .map_err(|_| ())
        .try_for_each(|chunk| {
            let tx = &tx;
            async move {
                match chunk.is_empty() {
                    true => Ok(()),
                    false => tx.send(chunk.to_vec()).await.map_err(|_| ()),
                }
            }
        })
        .await;
}

fn stalled(direction: &'static str) -> std::io::Error {
    std::io::Error::other(format!("lfs {direction} stalled past the idle timeout"))
}

struct MpscRead {
    runtime: Handle,
    rx: mpsc::Receiver<Vec<u8>>,
    buffer: Vec<u8>,
    offset: usize,
    waited: Duration,
    received: u64,
}

impl MpscRead {
    fn new(runtime: Handle, rx: mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            runtime,
            rx,
            buffer: Vec::new(),
            offset: 0,
            waited: Duration::ZERO,
            received: 0,
        }
    }
}

impl std::io::Read for MpscRead {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.offset >= self.buffer.len() {
            let started = std::time::Instant::now();
            let rx = &mut self.rx;
            let received = self
                .runtime
                // I know I know, but these aren't runtime workers here
                .block_on(async { tokio::time::timeout(LFS_STALL_TIMEOUT, rx.recv()).await });
            match received {
                Ok(Some(chunk)) => {
                    self.waited += started.elapsed();
                    self.received += chunk.len() as u64;
                    if !lfs_within_progress_budget(self.waited, self.received) {
                        return Err(std::io::Error::other(
                            "lfs input trickles below the progress floor",
                        ));
                    }
                    self.buffer = chunk;
                    self.offset = 0;
                }
                Ok(None) => return Ok(0),
                Err(_) => return Err(stalled("input")),
            }
        }
        let take = out.len().min(self.buffer.len() - self.offset);
        out[..take].copy_from_slice(&self.buffer[self.offset..self.offset + take]);
        self.offset += take;
        Ok(take)
    }
}

struct MeteredWrite<W> {
    runtime: Handle,
    inner: W,
    waited: Duration,
    written: u64,
}

impl<W: AsyncWrite + Unpin> MeteredWrite<W> {
    fn new(runtime: Handle, inner: W) -> Self {
        Self {
            runtime,
            inner,
            waited: Duration::ZERO,
            written: 0,
        }
    }

    fn charge(&mut self, started: std::time::Instant) -> std::io::Result<()> {
        self.waited += started.elapsed();
        match lfs_within_progress_budget(self.waited, self.written) {
            true => Ok(()),
            false => Err(std::io::Error::other(
                "lfs output trickles below the progress floor",
            )),
        }
    }
}

impl<W: AsyncWrite + Unpin> std::io::Write for MeteredWrite<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let started = std::time::Instant::now();
        let inner = &mut self.inner;
        let wrote = self
            .runtime
            .block_on(async { tokio::time::timeout(LFS_STALL_TIMEOUT, inner.write(buf)).await })
            .map_err(|_| stalled("output"))??;
        self.written += wrote as u64;
        self.charge(started).map(|()| wrote)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let started = std::time::Instant::now();
        let inner = &mut self.inner;
        self.runtime
            .block_on(async { tokio::time::timeout(LFS_STALL_TIMEOUT, inner.flush()).await })
            .map_err(|_| stalled("output"))??;
        self.charge(started)
    }
}

async fn serve_upload_archive<H: HttpTransport, C: Clock>(
    state: Arc<SshState<H, C>>,
    mut channel: Channel<Msg>,
    repo_did: RepoDid,
) {
    let request = {
        let mut reader = channel.make_reader();
        tokio::time::timeout(ARCHIVE_REQUEST_DEADLINE, read_archive_request(&mut reader)).await
    };
    let request = match request {
        Ok(Ok(request)) => request,
        Ok(Err(())) => return fail(channel, &state.catalog.ssh.archive_malformed.text()).await,
        Err(_) => return fail(channel, &state.catalog.ssh.archive_timeout.text()).await,
    };

    let permit = state.slots.pack.acquire().await;
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(16);
    let layout = state.layout.clone();
    let did = repo_did.clone();
    let archive_limit = state.archive_limit;
    let handle = tokio::task::spawn_blocking(move || -> Result<(), PackError> {
        let _permit = permit;
        let repo = layout.open(&did)?;
        let mut sink = |chunk: &[u8]| -> std::io::Result<()> {
            tx.blocking_send(chunk.to_vec())
                .map_err(|_| std::io::Error::other("client disconnected"))
        };
        knot_pack::upload_archive_streamed(&repo, &request, archive_limit, &mut sink)
    });

    let mut writer = channel.make_writer();
    let mut forward = Ok(());
    while let Some(chunk) = rx.recv().await {
        if writer.write_all(&chunk).await.is_err() {
            forward = Err(());
            break;
        }
    }
    drop(rx);
    let produced = handle.await;
    match &produced {
        Ok(Err(error)) => {
            tracing::warn!(repo = repo_did.as_str(), %error, "upload-archive failed")
        }
        Err(join) => {
            tracing::error!(repo = repo_did.as_str(), %join, "upload-archive task panicked")
        }
        Ok(Ok(())) => {}
    }
    match (forward, produced) {
        (Ok(()), Ok(Ok(()))) if writer.flush().await.is_ok() => finish(channel, 0).await,
        _ => fail(channel, &state.catalog.ssh.archive_failed.text()).await,
    }
}

async fn read_archive_request<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>, ()> {
    let mut buf = Vec::new();
    loop {
        if knot_pack::archive_request_complete(&buf).is_some() {
            return Ok(buf);
        }
        match read_chunk(reader, &mut buf, MAX_UPLOAD_REQUEST).await {
            Ok(true) => {}
            Ok(false) | Err(_) => return Err(()),
        }
    }
}

async fn serve_upload<H: HttpTransport, C: Clock>(
    state: Arc<SshState<H, C>>,
    mut channel: Channel<Msg>,
    repo_did: RepoDid,
    protocol_v2: bool,
) {
    let advert = {
        let layout = state.layout.clone();
        let did = repo_did.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<u8>, PackError> {
            let repo = layout.open(&did)?;
            if protocol_v2 {
                knot_pack::advertise_upload_ssh(&repo)
            } else {
                knot_pack::advertise_upload_v0_ssh(&repo)
            }
        })
        .await
    };
    let advert = match advert {
        Ok(Ok(bytes)) => bytes,
        _ => return fail(channel, &state.catalog.ssh.advertise_failed.text()).await,
    };

    let mut writer = channel.make_writer();
    if writer.write_all(&advert).await.is_err() || writer.flush().await.is_err() {
        return;
    }

    let outcome = {
        let mut reader = channel.make_reader();
        if protocol_v2 {
            upload_loop_v2(&state, &repo_did, &mut reader, &mut writer).await
        } else {
            upload_loop_v0(&state, &repo_did, &mut reader, &mut writer).await
        }
    };
    let status = match outcome {
        Ok(()) => 0,
        Err(()) => 1,
    };
    finish(channel, status).await;
}

async fn upload_loop_v2<H, C, R, W>(
    state: &Arc<SshState<H, C>>,
    repo_did: &RepoDid,
    reader: &mut R,
    writer: &mut W,
) -> Result<(), ()>
where
    H: HttpTransport,
    C: Clock,
    R: AsyncRead + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut buf = Vec::new();
    let mut framer = knot_pack::UploadFramer::new();
    loop {
        if let Some(len) = framer.advance(&buf) {
            let request: Vec<u8> = buf.drain(..len).collect();
            stream_upload(state, repo_did, request, writer).await?;
            framer = knot_pack::UploadFramer::new();
            continue;
        }
        match read_chunk(reader, &mut buf, MAX_UPLOAD_REQUEST).await {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(_) => return Err(()),
        }
    }
}

async fn upload_loop_v0<H, C, R, W>(
    state: &Arc<SshState<H, C>>,
    repo_did: &RepoDid,
    reader: &mut R,
    writer: &mut W,
) -> Result<(), ()>
where
    H: HttpTransport,
    C: Clock,
    R: AsyncRead + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut buf = Vec::new();
    let mut framer = knot_pack::UploadFramer::new();
    let mut naks_sent = 0usize;
    loop {
        if let Some(len) = framer.advance(&buf) {
            let request: Vec<u8> = buf.drain(..len).collect();
            return stream_upload(state, repo_did, request, writer).await;
        }
        let needed = framer.unanswered_flushes();
        if naks_sent < needed {
            let nak = knot_pack::upload_v0_nak();
            if writer.write_all(&nak).await.is_err() || writer.flush().await.is_err() {
                return Err(());
            }
            naks_sent += 1;
            continue;
        }
        match read_chunk(reader, &mut buf, MAX_UPLOAD_REQUEST).await {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(_) => return Err(()),
        }
    }
}

async fn stream_upload<H, C, W>(
    state: &Arc<SshState<H, C>>,
    repo_did: &RepoDid,
    request: Vec<u8>,
    writer: &mut W,
) -> Result<(), ()>
where
    H: HttpTransport,
    C: Clock,
    W: AsyncWriteExt + Unpin,
{
    let permit = state.slots.pack.acquire().await;
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(16);
    let layout = state.layout.clone();
    let did = repo_did.clone();
    let catalog = Arc::clone(&state.catalog);
    let knot = state.hostname.clone();
    let handle = tokio::task::spawn_blocking(move || -> Result<(), PackError> {
        let _permit = permit;
        let repo = layout.open(&did)?;
        let mut sink = |chunk: &[u8]| -> std::io::Result<()> {
            tx.blocking_send(chunk.to_vec())
                .map_err(|_| std::io::Error::other("client disconnected"))
        };
        knot_pack::upload_pack_streamed(&repo, &request, &catalog.fetch, &knot, &mut sink)
    });

    let mut forward = Ok(());
    while let Some(chunk) = rx.recv().await {
        if writer.write_all(&chunk).await.is_err() {
            forward = Err(());
            break;
        }
    }
    drop(rx);
    match (forward, handle.await) {
        (Ok(()), Ok(Ok(()))) => writer.flush().await.map_err(|_| ()),
        _ => Err(()),
    }
}

async fn serve_receive<H: HttpTransport, C: Clock>(
    state: Arc<SshState<H, C>>,
    credential: Credential,
    mut channel: Channel<Msg>,
    repo_did: RepoDid,
    peer: Option<IpAddr>,
) {
    let advert = {
        let layout = state.layout.clone();
        let did = repo_did.clone();
        tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, ObjectFormat), PackError> {
            let repo = layout.open(&did)?;
            let bytes = knot_pack::advertise_receive_ssh(&repo)?;
            Ok((bytes, repo.object_format()))
        })
        .await
    };
    let (advert, object_format) = match advert {
        Ok(Ok(pair)) => pair,
        _ => return fail(channel, &state.catalog.ssh.advertise_failed.text()).await,
    };

    let mut writer = channel.make_writer();
    if writer.write_all(&advert).await.is_err() || writer.flush().await.is_err() {
        return;
    }

    let committer = match authorize_push(&state, &credential, &repo_did, peer).await {
        PushAuth::Allowed(did) => did,
        PushAuth::Refused { reason, message } => {
            tracing::warn!(repo = repo_did.as_str(), reason, "ssh push denied");
            return fail(channel, &message).await;
        }
    };

    let _receive_permit = state.slots.receive.acquire().await;

    let limits = state.limits;
    let body = {
        let mut reader = channel.make_reader();
        let dir = state.layout.scratch_dir().to_path_buf();
        match tokio::time::timeout(
            RECEIVE_BODY_DEADLINE,
            read_receive(
                &mut reader,
                dir,
                state.max_pack_bytes,
                limits,
                object_format,
            ),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(ReadError::Deadline),
        }
    };
    let body = match body {
        Ok(body) => body,
        Err(ReadError::TooLarge) => {
            return fail(channel, &state.catalog.ssh.push_too_large.text()).await;
        }
        Err(ReadError::Deadline) => {
            return fail(channel, &state.catalog.ssh.receive_deadline.text()).await;
        }
        Err(ReadError::Pack(error)) => {
            tracing::warn!(repo = repo_did.as_str(), %error, "receive framing failed");
            return fail(channel, &state.catalog.ssh.malformed_pack.text()).await;
        }
        Err(ReadError::Io(error)) => {
            tracing::warn!(repo = repo_did.as_str(), %error, "receive read error");
            return fail(channel, &state.catalog.ssh.receive_read_error.text()).await;
        }
        Err(ReadError::Truncated) => {
            return fail(channel, &state.catalog.ssh.receive_ended_early.text()).await;
        }
    };
    if body.is_empty() {
        return finish(channel, 0).await;
    }

    let _pack_permit = state.slots.pack.acquire().await;
    let landed = knot_receive::land(knot_receive::Push {
        layout: &state.layout,
        repo_did: &repo_did,
        received: body,
        limits: state.limits,
        knot_actor: state.knot_actor.clone(),
        committer,
        events: Arc::clone(&state.events),
        index: &state.index,
        atproto: &state.atproto,
        resolve_slots: &state.slots.resolve,
        appview: &state.appview,
        maintenance: &state.maintenance,
        hostname: &state.hostname,
        languages_push_budget: state.languages_push_budget,
        catalog: Arc::clone(&state.catalog),
        ci_logs: state.ci_logs.clone(),
    })
    .await;
    match landed {
        Ok(framed) => {
            let _ = writer.write_all(&framed).await;
            let _ = writer.flush().await;
            finish(channel, 0).await;
        }
        Err(error) => {
            tracing::warn!(repo = repo_did.as_str(), %error, "receive-pack failed");
            fail(channel, &state.catalog.ssh.receive_failed.text()).await;
        }
    }
}

pub(crate) async fn run_greeting<H: HttpTransport, C: Clock>(
    state: Arc<SshState<H, C>>,
    credential: Credential,
    channel: Channel<Msg>,
) {
    let knot = state.hostname.as_str().to_string();
    let greeting = match greeting_visitor(&state, &credential).await {
        Visitor::Named(who) => state.catalog.ssh.greeting.lines(|key| match key {
            knot_messages::GreetingKey::User => who.clone(),
            knot_messages::GreetingKey::Knot => knot.clone(),
        }),
        Visitor::Unknown => state
            .catalog
            .ssh
            .greeting_unknown
            .lines(|knot_messages::KnotKey::Knot| knot.clone()),
    };
    if greeting.is_empty() {
        return finish(channel, 0).await;
    }
    let body = greeting.join("\r\n");
    let _ = channel
        .extended_data_bytes(1, format!("{body}\r\n").into_bytes())
        .await;
    finish(channel, 0).await;
}

async fn greeting_visitor<H: HttpTransport, C: Clock>(
    state: &Arc<SshState<H, C>>,
    credential: &Credential,
) -> Visitor {
    let did = match credential {
        Credential::Identified(did) => did.clone(),
        Credential::Offered(key) => {
            match state.index.owner_of_key(key, state.atproto.now().seconds()) {
                Resolved::Ready(Some(did)) => did,
                _ => return Visitor::Unknown,
            }
        }
    };
    match knot_receive::resolve_handle(&state.atproto, &state.slots.resolve, &did).await {
        Some(handle) => Visitor::Named(format!("@{}", handle.as_str())),
        None => Visitor::Named(did.as_str().to_string()),
    }
}

enum PusherLookup {
    Matched(AccountDid),
    Unmatched(Vec<AccountDid>),
    Unavailable,
}

enum PushAuth {
    Allowed(AccountDid),
    Refused {
        reason: &'static str,
        message: String,
    },
}

enum Visitor {
    Named(String),
    Unknown,
}

async fn authorize_push<H: HttpTransport, C: Clock>(
    state: &Arc<SshState<H, C>>,
    credential: &Credential,
    repo: &RepoDid,
    peer: Option<IpAddr>,
) -> PushAuth {
    match resolve_pusher(state, credential, repo, peer).await {
        PusherLookup::Matched(did) => {
            let acl = KnotAcl::new(&state.admins, state.admission, &state.index);
            match can_push(&acl, &did, repo).is_allowed() {
                true => PushAuth::Allowed(did),
                false => PushAuth::Refused {
                    reason: "unauthorized",
                    message: state.catalog.ssh.push_denied.text(),
                },
            }
        }
        PusherLookup::Unavailable => PushAuth::Refused {
            reason: "identity_unavailable",
            message: state.catalog.ssh.identity_unavailable.text(),
        },
        PusherLookup::Unmatched(candidates) => {
            let authorized = describe_authorized(state, &candidates).await;
            PushAuth::Refused {
                reason: "unregistered_key",
                message: state
                    .catalog
                    .ssh
                    .key_not_registered
                    .line(|knot_messages::AuthorizedKey::Authorized| authorized.clone()),
            }
        }
    }
}

async fn describe_authorized<H: HttpTransport, C: Clock>(
    state: &Arc<SshState<H, C>>,
    candidates: &[AccountDid],
) -> String {
    let names: Vec<String> = futures::stream::iter(
        candidates
            .iter()
            .take(AUTHORIZED_NAMES_SHOWN)
            .cloned()
            .collect::<Vec<_>>(),
    )
    .map(|did| {
        let state = Arc::clone(state);
        async move {
            match knot_receive::resolve_handle(&state.atproto, &state.slots.resolve, &did).await {
                Some(handle) => format!("@{}", handle.as_str()),
                None => did.as_str().to_string(),
            }
        }
    })
    .buffered(CANDIDATE_FANOUT)
    .collect()
    .await;
    match (
        names.as_slice(),
        candidates.len().saturating_sub(AUTHORIZED_NAMES_SHOWN),
    ) {
        ([], _) => "nobody".to_string(),
        (shown, 0) => shown.join(", "),
        (shown, hidden) => format!("{}, and {hidden} more", shown.join(", ")),
    }
}

fn probe_due<H: HttpTransport, C: Clock>(state: &Arc<SshState<H, C>>, did: &AccountDid) -> bool {
    state
        .probe_pace
        .reserve_now(&SubjectKey::new(did.as_str()), state.atproto.now())
}

async fn resolve_pusher<H: HttpTransport, C: Clock>(
    state: &Arc<SshState<H, C>>,
    credential: &Credential,
    repo: &RepoDid,
    peer: Option<IpAddr>,
) -> PusherLookup {
    let key = match credential {
        Credential::Identified(did) => return PusherLookup::Matched(did.clone()),
        Credential::Offered(key) => key,
    };
    let owner = match state.index.owner_of(repo) {
        Resolved::Ready(Some(owner)) => Some(AccountDid::from(owner)),
        _ => None,
    };
    {
        let index = Arc::clone(&state.index);
        let target = repo.clone();
        let _ = tokio::task::spawn_blocking(move || index.ensure_collaborators(&target)).await;
    }
    let collaborators = match state.index.collaborators_of(repo) {
        Resolved::Ready(collaborators) => collaborators,
        _ => Vec::new(),
    };
    let candidates: Vec<AccountDid> = owner.into_iter().chain(collaborators).collect();
    let now = state.atproto.now().seconds();
    if let Some(publisher) = state.index.keys().publisher_among(&candidates, key, now) {
        return PusherLookup::Matched(publisher);
    }
    let unread: Vec<AccountDid> = candidates
        .iter()
        .filter(|did| !state.index.keys().is_fresh(did, now) || probe_due(state, did))
        .cloned()
        .collect();
    if unread.is_empty() {
        tracing::debug!(
            ?peer,
            repo = repo.as_str(),
            candidates = candidates.len(),
            "push check has every candidate's keys on file, and the candidates don't publish \
             the offered key"
        );
        return PusherLookup::Unmatched(candidates);
    }
    let lease = state.key_ttl.lease_from(now);
    let unresolved = Arc::new(AtomicBool::new(false));
    let read: Vec<Option<AccountDid>> = futures::stream::iter(unread)
        .map(|did| {
            let state = Arc::clone(state);
            let key = key.clone();
            let unresolved = Arc::clone(&unresolved);
            async move {
                let _permit = state.slots.resolve.acquire().await;
                match state.atproto.resolve_pubkeys(&did).await {
                    Ok(keys) => {
                        let matches = keys.contains(&key);
                        state.index.keys().record(&did, keys, lease);
                        matches.then_some(did)
                    }
                    Err(error) if error.is_gone() => {
                        tracing::debug!(
                            did = did.as_str(),
                            %error,
                            "push check records an empty key set for a candidate whose DID document is gone"
                        );
                        state.index.keys().record(&did, Vec::new(), lease);
                        None
                    }
                    Err(error) => {
                        tracing::debug!(
                            did = did.as_str(),
                            %error,
                            "push check couldn't read a candidate's records"
                        );
                        unresolved.store(true, Ordering::Relaxed);
                        None
                    }
                }
            }
        })
        .buffered(CANDIDATE_FANOUT)
        .collect()
        .await;
    match read.into_iter().flatten().next() {
        Some(did) => PusherLookup::Matched(did),
        None if unresolved.load(Ordering::Relaxed) => PusherLookup::Unavailable,
        None => PusherLookup::Unmatched(candidates),
    }
}

async fn read_chunk<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    limit: usize,
) -> Result<bool, ReadError> {
    let mut chunk = [0u8; READ_CHUNK];
    let read = reader.read(&mut chunk).await.map_err(ReadError::Io)?;
    if read == 0 {
        return Ok(false);
    }
    buf.extend_from_slice(&chunk[..read]);
    if buf.len() > limit {
        return Err(ReadError::TooLarge);
    }
    Ok(true)
}

async fn read_receive<R: AsyncRead + Unpin>(
    reader: &mut R,
    dir: PathBuf,
    limit: knot_pack::MaxWireBytes,
    limits: PackLimits,
    format: ObjectFormat,
) -> Result<knot_pack::ReceivedPack, ReadError> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>(8);
    let mut framer =
        tokio::task::spawn_blocking(move || frame_receive(rx, dir, limit, limits, format));
    let mut chunk = [0u8; READ_CHUNK];
    let mut io_error = None;
    loop {
        tokio::select! {
            biased;
            framed = &mut framer => return join_framed(framed, io_error),
            read = reader.read(&mut chunk) => match read {
                Ok(0) => break,
                Ok(read) => {
                    if tx.send(chunk[..read].to_vec()).await.is_err() {
                        break;
                    }
                }
                Err(error) => {
                    io_error = Some(error);
                    break;
                }
            },
        }
    }
    drop(tx);
    join_framed(framer.await, io_error)
}

fn join_framed(
    framed: Result<Result<knot_pack::ReceivedPack, ReadError>, tokio::task::JoinError>,
    io_error: Option<std::io::Error>,
) -> Result<knot_pack::ReceivedPack, ReadError> {
    match framed {
        Ok(Ok(body)) => Ok(body),
        Ok(Err(ReadError::Truncated)) => {
            Err(io_error.map(ReadError::Io).unwrap_or(ReadError::Truncated))
        }
        Ok(Err(other)) => Err(other),
        Err(_) => Err(ReadError::Truncated),
    }
}

fn read_error(error: knot_pack::ReceiveReadError) -> ReadError {
    match error {
        knot_pack::ReceiveReadError::Io(error) => ReadError::Io(error),
        knot_pack::ReceiveReadError::Pack(error) => ReadError::Pack(error),
        knot_pack::ReceiveReadError::TooLarge => ReadError::TooLarge,
        knot_pack::ReceiveReadError::Truncated => ReadError::Truncated,
    }
}

fn frame_receive(
    mut rx: mpsc::Receiver<Vec<u8>>,
    dir: PathBuf,
    limit: knot_pack::MaxWireBytes,
    limits: PackLimits,
    format: ObjectFormat,
) -> Result<knot_pack::ReceivedPack, ReadError> {
    let mut receiver =
        knot_pack::PackReceiver::new(&dir, limit, limits, format.kind()).map_err(ReadError::Io)?;
    loop {
        match rx.blocking_recv() {
            Some(chunk) => {
                if receiver.write(&chunk).map_err(read_error)? {
                    return receiver.finish().map_err(read_error);
                }
            }
            None => return receiver.finish().map_err(read_error),
        }
    }
}

async fn fail(channel: Channel<Msg>, message: &str) {
    let _ = channel
        .extended_data_bytes(1, format!("{message}\n").into_bytes())
        .await;
    finish(channel, 1).await;
}

async fn finish(channel: Channel<Msg>, status: u32) {
    let _ = channel.exit_status(status).await;
    let _ = channel.eof().await;
    let _ = channel.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lfs_progress_budget_spares_slow_links_and_cuts_trickles() {
        assert!(lfs_within_progress_budget(Duration::from_secs(59), 0));
        assert!(!lfs_within_progress_budget(Duration::from_secs(61), 0));
        assert!(lfs_within_progress_budget(
            Duration::from_secs(50_000),
            5 * 1024 * 1024 * 1024
        ));
        assert!(!lfs_within_progress_budget(Duration::from_secs(1_000), 10));
    }

    #[test]
    fn the_repo_path_parser_separates_dids_from_handles() {
        assert!(matches!(
            parse_repo_path("did:plc:nel/squid"),
            Some(RepoRef::OwnerPath(..))
        ));
        assert!(matches!(
            parse_repo_path("nel.pet/squid"),
            Some(RepoRef::HandlePath(..))
        ));
        assert!(matches!(
            parse_repo_path("did:plc:barnacle"),
            Some(RepoRef::Did(_))
        ));
        assert!(parse_repo_path("did:nonsense/squid").is_none());
        assert!(parse_repo_path("nel.pet").is_none());
    }
}
