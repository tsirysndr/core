use crate::command::{self, Spec};
use crate::nix_config::{SPINDLE_RUN_DIR, clean_store_paths, nix_executable};
use crate::protocol::{Message, v1};
use anyhow::{Context, Result};
use nix::unistd::{Group, chown};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt::{self, Write as FmtWrite};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::net::Shutdown;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Semaphore, mpsc, oneshot, watch};
use tokio::task::{JoinError, JoinHandle, JoinSet};
use tokio_vsock::{VMADDR_CID_LOCAL, VsockAddr, VsockListener, VsockStream};
use tracing::{info, warn};

mod read_proxy;
mod write_proxy;

pub use read_proxy::ReadCacheProxy;
pub use write_proxy::WriteCacheProxy;

const UPLOAD_QUEUE_CAPACITY: usize = 128;
const CONNECTION_WORKERS: usize = 4;
const CACHE_ENQUEUE_IO_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_CACHE_ENQUEUE_PORT: u32 = 10241;
const SHUTTLE_CACHE_ENQUEUE_PORT_ENV: &str = "SHUTTLE_CACHE_VSOCK_PORT";
const NIX_BUILD_GROUP: &str = "nixbld";
const SPINDLE_HOOK_TOKEN: &str = "/run/spindle/hook-token";

#[derive(Clone, Debug, Default)]
pub struct CacheStats {
    pub pending: u32,
    pub active: u32,
    pub uploaded: u32,
    pub failed: u32,
    pub last_error: Option<String>,
}

#[derive(Debug, Default)]
struct CacheState {
    stats: CacheStats,
    enqueue_active: u32,
    stopped: bool,
}

impl CacheState {
    fn snapshot(&self) -> CacheSnapshot {
        CacheSnapshot {
            stats: self.stats.clone(),
            enqueue_active: self.enqueue_active,
            stopped: self.stopped,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct CacheSnapshot {
    stats: CacheStats,
    enqueue_active: u32,
    stopped: bool,
}

impl CacheSnapshot {
    fn is_idle(&self) -> bool {
        self.stats.pending == 0 && self.stats.active == 0 && self.enqueue_active == 0
    }
}

#[derive(Clone)]
pub struct CacheUploadManager {
    inner: Arc<CacheUploadInner>,
}

struct CacheUploadInner {
    cmd_tx: mpsc::Sender<Cmd>,
    stats_rx: watch::Receiver<CacheSnapshot>,
    handles: Mutex<Vec<JoinHandle<()>>>,
}

struct UploadJob {
    paths: Vec<String>,
    count: u32,
}

enum Cmd {
    EnqueueStarted,
    EnqueueFinished,
    Enqueue {
        paths: Vec<String>,
        reply: oneshot::Sender<Result<usize, String>>,
    },
    UploadStarted {
        count: u32,
    },
    UploadFinished {
        count: u32,
        error: Option<String>,
    },
    UploadWorkerStopped,
    Stop,
}

impl CacheUploadManager {
    pub async fn start(upload_url: &str, event_tx: mpsc::Sender<Message>) -> Result<Option<Self>> {
        if upload_url.is_empty() {
            // nothing to upload to, so don't require the guest-local vsock
            // listener (vsock_loopback) or the nix post-build hook
            info!("no cache upload url configured, cache uploads disabled");
            return Ok(None);
        }
        let token = create_hook_token().context("create cache hook token")?;
        let port = cache_enqueue_port();
        let listener = VsockListener::bind(VsockAddr::new(VMADDR_CID_LOCAL, port))
            .with_context(|| format!("listen on guest-local vsock port {port}"))?;

        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>(UPLOAD_QUEUE_CAPACITY);
        let (upload_tx, upload_rx) = mpsc::channel::<UploadJob>(UPLOAD_QUEUE_CAPACITY);
        let (stats_tx, stats_rx) = watch::channel(CacheSnapshot::default());

        let mut handles = Vec::with_capacity(3);

        handles.push(tokio::spawn(async move {
            cache_manager_loop(cmd_rx, upload_tx, stats_tx).await;
        }));

        let upload_cmd_tx = cmd_tx.clone();
        let upload_url = upload_url.to_owned();
        handles.push(tokio::spawn(async move {
            upload_loop(upload_rx, upload_cmd_tx, upload_url).await;
        }));

        let accept_cmd_tx = cmd_tx.clone();
        handles.push(tokio::spawn(async move {
            accept_loop(listener, token, event_tx, accept_cmd_tx).await;
        }));

        info!(
            port,
            workers = CONNECTION_WORKERS,
            "cache upload queue ready"
        );
        let inner = Arc::new(CacheUploadInner {
            cmd_tx,
            stats_rx,
            handles: Mutex::new(handles),
        });
        Ok(Some(Self { inner }))
    }

    pub async fn drain(&self, timeout: Option<Duration>) -> CacheStats {
        let mut stats_rx = self.inner.stats_rx.clone();
        let wait = async {
            loop {
                let snapshot = stats_rx.borrow_and_update().clone();
                if snapshot.is_idle() || snapshot.stopped {
                    return snapshot.stats;
                }
                if stats_rx.changed().await.is_err() {
                    let mut stats = stats_rx.borrow().stats.clone();
                    stats.last_error = Some("cache manager stopped".to_owned());
                    return stats;
                }
            }
        };

        match timeout {
            Some(timeout) => match tokio::time::timeout(timeout, wait).await {
                Ok(stats) => stats,
                Err(_) => {
                    let mut stats = self.inner.stats_rx.borrow().stats.clone();
                    stats.last_error = Some("cache drain timed out".to_owned());
                    stats
                }
            },
            None => wait.await,
        }
    }
}

impl Drop for CacheUploadInner {
    fn drop(&mut self) {
        let _ = self.cmd_tx.try_send(Cmd::Stop);
        if let Ok(mut handles) = self.handles.lock() {
            for handle in handles.drain(..) {
                handle.abort();
            }
        }
        let _ = fs::remove_file(SPINDLE_HOOK_TOKEN);
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct EnqueueBuiltPathsRequest {
    token: String,
    paths: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct EnqueueBuiltPathsResponse {
    queued: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    error: String,
}

#[derive(Debug)]
enum JsonLineError {
    Empty,
    TimedOut,
    Io(io::Error),
    Json(serde_json::Error),
}

impl JsonLineError {
    fn enqueue_request_message(self) -> String {
        match self {
            Self::Empty => "empty cache enqueue request".to_owned(),
            Self::TimedOut => "cache enqueue read timed out".to_owned(),
            Self::Io(error) => error.to_string(),
            Self::Json(error) => error.to_string(),
        }
    }
}

impl fmt::Display for JsonLineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("empty message"),
            Self::TimedOut => f.write_str("timed out"),
            Self::Io(error) => error.fmt(f),
            Self::Json(error) => error.fmt(f),
        }
    }
}

async fn cache_manager_loop(
    mut cmd_rx: mpsc::Receiver<Cmd>,
    upload_tx: mpsc::Sender<UploadJob>,
    stats_tx: watch::Sender<CacheSnapshot>,
) {
    let mut state = CacheState::default();

    let publish_state = |state: &CacheState| {
        let _ = stats_tx.send(state.snapshot());
    };

    while let Some(command) = cmd_rx.recv().await {
        match command {
            Cmd::EnqueueStarted => {
                state.enqueue_active += 1;
                publish_state(&state);
            }
            Cmd::EnqueueFinished => {
                decrement_counter(&mut state.enqueue_active, 1, "cache enqueue active");
                publish_state(&state);
            }
            Cmd::Enqueue { paths, reply } => {
                let result = enqueue_upload_job(&mut state, &upload_tx, paths);
                publish_state(&state);
                let _ = reply.send(result);
            }
            Cmd::UploadStarted { count } => {
                decrement_counter(&mut state.stats.pending, count, "cache pending uploads");
                state.stats.active += count;
                publish_state(&state);
            }
            Cmd::UploadFinished { count, error } => {
                decrement_counter(&mut state.stats.active, count, "cache active uploads");
                if let Some(error) = error {
                    state.stats.failed += count;
                    state.stats.last_error = Some(error);
                } else {
                    state.stats.uploaded += count;
                }
                publish_state(&state);
            }
            Cmd::UploadWorkerStopped => {
                state.stopped = true;
                publish_state(&state);
            }
            Cmd::Stop => {
                state.stopped = true;
                publish_state(&state);
                break;
            }
        }
    }

    state.stopped = true;
    publish_state(&state);
}

fn enqueue_upload_job(
    state: &mut CacheState,
    upload_tx: &mpsc::Sender<UploadJob>,
    paths: Vec<String>,
) -> Result<usize, String> {
    if state.stopped {
        return Err("cache manager stopped".to_owned());
    }

    let count = paths.len() as u32;
    if count == 0 {
        return Ok(0);
    }

    match upload_tx.try_send(UploadJob { paths, count }) {
        Ok(()) => {
            state.stats.pending += count;
            Ok(count as usize)
        }
        Err(mpsc::error::TrySendError::Full(_)) => Err("cache upload queue is full".to_owned()),
        Err(mpsc::error::TrySendError::Closed(_)) => {
            state.stopped = true;
            Err("cache upload worker stopped".to_owned())
        }
    }
}

fn decrement_counter(counter: &mut u32, count: u32, name: &'static str) {
    match counter.checked_sub(count) {
        Some(value) => *counter = value,
        None => {
            warn!(name, current = *counter, count, "cache counter underflow");
            *counter = 0;
        }
    }
}

async fn upload_loop(
    mut upload_rx: mpsc::Receiver<UploadJob>,
    cmd_tx: mpsc::Sender<Cmd>,
    upload_url: String,
) {
    while let Some(job) = upload_rx.recv().await {
        let cmd = Cmd::UploadStarted { count: job.count };
        if cmd_tx.send(cmd).await.is_err() {
            break;
        }

        let error = upload_paths(&upload_url, &job.paths)
            .await
            .err()
            .map(|error| error.to_string());

        let cmd = Cmd::UploadFinished {
            count: job.count,
            error,
        };
        if cmd_tx.send(cmd).await.is_err() {
            break;
        }
    }

    let _ = cmd_tx.send(Cmd::UploadWorkerStopped).await;
}

// runs nix copy against the write cache proxy, which goes to the spindle
// and spindle will then forward the request to the actual binary cache
async fn upload_paths(upload_url: &str, paths: &[String]) -> Result<()> {
    fn add_query_param(url: &str, key: &str, value: &str) -> String {
        let separator = url.contains('?').then_some('&').unwrap_or('?');
        format!("{}{}{}={}", url, separator, key, value)
    }

    if paths.is_empty() || upload_url.is_empty() {
        return Ok(());
    }

    // we use zstd 3 because its the best usually. it is faster than no
    // compression also because of IO savings
    let dest_url = add_query_param(upload_url, "compression", "zstd");
    let dest_url = add_query_param(&dest_url, "compression-level", "3");
    let dest_url = add_query_param(&dest_url, "parallel-compression", "true");

    let spec = Spec::new(nix_executable())
        .args(["copy", "--to", &dest_url])
        .args(paths.iter().cloned())
        .timeout(Duration::from_secs(10 * 60));

    let output = command::run_capture(spec).await.context("run nix copy")?;
    if !output.success() {
        anyhow::bail!(
            "nix copy failed: exit={} error={:?} output={}",
            output.exit.exit_code,
            output.exit.error,
            output.combined_lossy(),
        );
    }

    info!(paths = paths.len(), %upload_url, "uploaded cache paths");
    Ok(())
}

async fn accept_loop(
    listener: VsockListener,
    token: String,
    event_tx: mpsc::Sender<Message>,
    cmd_tx: mpsc::Sender<Cmd>,
) {
    let permits = Arc::new(Semaphore::new(CONNECTION_WORKERS));
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((conn, _addr)) => {
                    let Ok(permit) = permits.clone().try_acquire_owned() else {
                        tasks.spawn(async move {
                            let mut conn = conn;
                            write_enqueue_response(
                                &mut conn,
                                0,
                                Some("cache enqueue workers are busy".to_owned()),
                            )
                            .await;
                        });
                        warn!("cache enqueue dropped because workers are busy");
                        continue;
                    };

                    let worker_cmd_tx = cmd_tx.clone();
                    if let Err(error) = start_enqueue_request(&worker_cmd_tx) {
                        tasks.spawn(async move {
                            let mut conn = conn;
                            write_enqueue_response(&mut conn, 0, Some(error)).await;
                        });
                        continue;
                    }

                    let worker_token = token.clone();
                    let worker_event_tx = event_tx.clone();
                    tasks.spawn(async move {
                        let _permit = permit;
                        handle_enqueue_conn(
                            conn,
                            &worker_token,
                            &worker_event_tx,
                            &worker_cmd_tx,
                        )
                        .await;
                        let _ = worker_cmd_tx.send(Cmd::EnqueueFinished).await;
                    });
                }
                Err(error) => {
                    if error.kind() != io::ErrorKind::Interrupted {
                        warn!(%error, "cache enqueue accept failed");
                    }
                }
            },
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                log_enqueue_task_result(result);
            }
        }
    }
}

fn log_enqueue_task_result(result: Result<(), JoinError>) {
    if let Err(error) = result {
        warn!(%error, "cache enqueue task failed");
    }
}

async fn handle_enqueue_conn(
    mut conn: VsockStream,
    expected_token: &str,
    event_tx: &mpsc::Sender<Message>,
    cmd_tx: &mpsc::Sender<Cmd>,
) {
    let req: EnqueueBuiltPathsRequest = match read_enqueue_request(&mut conn).await {
        Ok(req) => req,
        Err(error) => {
            write_enqueue_response(&mut conn, 0, Some(error)).await;
            return;
        }
    };

    if req.token != expected_token {
        write_enqueue_response(&mut conn, 0, Some("invalid cache enqueue token".to_owned())).await;
        return;
    }

    match enqueue_paths(cmd_tx, req.paths).await {
        Ok(queued) => {
            send_built_paths_event(event_tx, queued.event_paths).await;
            write_enqueue_response(&mut conn, queued.count, None).await;
        }
        Err(error) => write_enqueue_response(&mut conn, 0, Some(error)).await,
    }
}

fn start_enqueue_request(cmd_tx: &mpsc::Sender<Cmd>) -> Result<(), String> {
    match cmd_tx.try_send(Cmd::EnqueueStarted) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(_)) => Err("cache upload queue is full".to_owned()),
        Err(mpsc::error::TrySendError::Closed(_)) => Err("cache upload worker stopped".to_owned()),
    }
}

async fn read_enqueue_request(conn: &mut VsockStream) -> Result<EnqueueBuiltPathsRequest, String> {
    read_json_line(conn)
        .await
        .map_err(JsonLineError::enqueue_request_message)
}

async fn read_json_line<T>(conn: &mut VsockStream) -> Result<T, JsonLineError>
where
    T: DeserializeOwned,
{
    let mut data = Vec::new();
    let mut reader = BufReader::new(conn);
    let bytes_read = tokio::time::timeout(
        CACHE_ENQUEUE_IO_TIMEOUT,
        reader.read_until(b'\n', &mut data),
    )
    .await
    .map_err(|_| JsonLineError::TimedOut)?
    .map_err(JsonLineError::Io)?;

    if bytes_read == 0 {
        return Err(JsonLineError::Empty);
    }

    serde_json::from_slice(&data).map_err(JsonLineError::Json)
}

struct QueuedPaths {
    count: usize,
    event_paths: Vec<String>,
}

async fn enqueue_paths(
    cmd_tx: &mpsc::Sender<Cmd>,
    paths: Vec<String>,
) -> Result<QueuedPaths, String> {
    let paths = clean_store_paths(&paths);
    let event_paths = paths.clone();
    if paths.is_empty() {
        return Ok(QueuedPaths {
            count: 0,
            event_paths,
        });
    }

    let (reply, queued) = oneshot::channel();
    match cmd_tx.try_send(Cmd::Enqueue { paths, reply }) {
        Ok(()) => match queued.await {
            Ok(Ok(count)) => Ok(QueuedPaths { count, event_paths }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err("cache upload worker stopped".to_owned()),
        },
        Err(mpsc::error::TrySendError::Full(_)) => Err("cache upload queue is full".to_owned()),
        Err(mpsc::error::TrySendError::Closed(_)) => Err("cache upload worker stopped".to_owned()),
    }
}

async fn send_built_paths_event(event_tx: &mpsc::Sender<Message>, paths: Vec<String>) {
    if paths.is_empty() {
        return;
    }

    let msg = Message {
        id: "built-paths".to_owned(),
        built_paths: Some(v1::BuiltPaths {
            paths,
            reason: "post_build_hook".to_owned(),
        }),
        ..Default::default()
    };
    let _ = event_tx.send(msg).await;
}

async fn write_enqueue_response(conn: &mut VsockStream, queued: usize, error: Option<String>) {
    let response = EnqueueBuiltPathsResponse {
        queued,
        error: error.unwrap_or_default(),
    };
    let _ = write_json_line(conn, &response).await;
}

async fn write_json_line<T>(conn: &mut VsockStream, value: &T) -> Result<(), JsonLineError>
where
    T: Serialize + ?Sized,
{
    let data = serde_json::to_vec(value).map_err(JsonLineError::Json)?;
    tokio::time::timeout(CACHE_ENQUEUE_IO_TIMEOUT, async {
        conn.write_all(&data).await?;
        conn.write_all(b"\n").await?;
        VsockStream::shutdown(conn, Shutdown::Write)
    })
    .await
    .map_err(|_| JsonLineError::TimedOut)?
    .map_err(JsonLineError::Io)
}

// we use a loopback vsock here since its better than having to do the whole http song and dance!
pub async fn enqueue_built_paths(paths: &[String]) {
    let paths = clean_store_paths(paths);
    if paths.is_empty() {
        return;
    }

    let token = match read_hook_token() {
        Ok(token) => token,
        Err(_) => return,
    };

    if token.is_empty() {
        return;
    }

    let mut conn =
        match VsockStream::connect(VsockAddr::new(VMADDR_CID_LOCAL, cache_enqueue_port())).await {
            Ok(conn) => conn,
            Err(error) => {
                warn!(paths = paths.len(), %error, "cache enqueue unavailable");
                return;
            }
        };

    let request = EnqueueBuiltPathsRequest { token, paths };
    match write_json_line(&mut conn, &request).await {
        Ok(()) => {}
        Err(JsonLineError::Json(error)) => {
            warn!(%error, "cache enqueue encode failed");
            return;
        }
        Err(JsonLineError::TimedOut) => {
            warn!("cache enqueue write timed out");
            return;
        }
        Err(error) => {
            warn!(%error, "cache enqueue write failed");
            return;
        }
    }

    let response: EnqueueBuiltPathsResponse = match read_json_line(&mut conn).await {
        Ok(response) => response,
        Err(JsonLineError::Empty) => {
            warn!("cache enqueue ack was empty");
            return;
        }
        Err(JsonLineError::TimedOut) => {
            warn!("cache enqueue ack timed out");
            return;
        }
        Err(error) => {
            warn!(%error, "cache enqueue ack failed");
            return;
        }
    };

    if !response.error.is_empty() {
        warn!(error = %response.error, "cache enqueue rejected");
        return;
    }

    info!(queued = response.queued, "cache paths enqueued");
}

fn cache_enqueue_port() -> u32 {
    std::env::var(SHUTTLE_CACHE_ENQUEUE_PORT_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_CACHE_ENQUEUE_PORT)
}

fn create_hook_token() -> Result<String> {
    use std::io::Write;

    fs::create_dir_all(SPINDLE_RUN_DIR).with_context(|| format!("create {SPINDLE_RUN_DIR}"))?;
    let token = random_token().context("generate hook token")?;
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o640)
        .open(SPINDLE_HOOK_TOKEN)
        .with_context(|| format!("create {SPINDLE_HOOK_TOKEN}"))?;
    allow_nix_build_group(SPINDLE_HOOK_TOKEN)?;
    file.write_all(token.as_bytes())
        .with_context(|| format!("write {SPINDLE_HOOK_TOKEN}"))?;
    file.write_all(b"\n")
        .with_context(|| format!("write {SPINDLE_HOOK_TOKEN}"))?;
    Ok(token)
}

fn allow_nix_build_group(path: &str) -> Result<()> {
    let Some(group) =
        Group::from_name(NIX_BUILD_GROUP).with_context(|| format!("lookup {NIX_BUILD_GROUP}"))?
    else {
        warn!(
            group = NIX_BUILD_GROUP,
            "nix build group not found; cache hook token remains root-only"
        );
        return Ok(());
    };

    chown(path, None, Some(group.gid)).with_context(|| format!("chown {path} to {NIX_BUILD_GROUP}"))
}

fn read_hook_token() -> Result<String> {
    fs::read_to_string(SPINDLE_HOOK_TOKEN)
        .map(|token| token.trim().to_owned())
        .with_context(|| format!("read {SPINDLE_HOOK_TOKEN}"))
}

fn random_token() -> Result<String> {
    let mut bytes = [0_u8; 32];
    File::open("/dev/urandom")
        .context("open /dev/urandom")?
        .read_exact(&mut bytes)
        .context("read /dev/urandom")?;

    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut token, "{byte:02x}").unwrap();
    }
    Ok(token)
}
