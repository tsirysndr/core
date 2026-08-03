mod exec;
mod identity;
mod server;

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use knot_atproto::Atproto;
use knot_events::EventLog;
use knot_git::{ArchiveLimit, Layout};
use knot_index::{Index, KeyTtl};
use knot_maintenance::MaintenanceHandle;
use knot_pack::{MaxWireBytes, PackLimits};
use knot_postreceive::LanguagesPushBudget;
use knot_runtime::{Clock, Entropy, HttpTransport, OsEntropy};
use knot_types::{AccountDid, ActorId, AdmissionPolicy, AppviewEndpoint, CiLogsAddr, KnotHostname};
use russh::keys::ssh_key::rand_core;
use russh::keys::{Algorithm, PrivateKey, ssh_key};
use russh::server::{Config, Server as _};
use russh::{MethodKind, MethodSet, Preferred, compression};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use knot_resource::{
    Burst, GlobalInflight, LimitConfig, PeerPacer, PerPeerInflight, PreAuthLimiter, RateLimit,
    RefillMicros, ResolveSlots, Slots, SubjectPacer,
};
use server::KnotSshServer;

const MAX_INFLIGHT_PER_PEER: usize = 4;
const MAX_INFLIGHT_LOOKUPS: usize = 16;
const MAX_PREAUTH_LOOKUPS: usize = 4;
const LOOKUP_BURST_PER_PEER: u32 = 8;
const LOOKUP_REFILL_MICROS: u64 = 500_000;
const LOOKUP_INFLIGHT_PER_PEER: usize = 2;
const PROBE_BURST_PER_ACCOUNT: u32 = 1;
const PROBE_REFILL_MICROS: u64 = 30_000_000;
const MISS_BURST_PER_PEER: u32 = 1;
const MISS_REFILL_MICROS: u64 = 120_000_000;
const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(120);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const AUTH_REJECTION_TIME: Duration = Duration::from_millis(250);
const DRAIN_GRACE: Duration = Duration::from_secs(30);
const ACCEPT_BACKOFF: Duration = Duration::from_millis(250);

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("ssh host key {path}: {message}")]
    HostKey { path: String, message: String },
    #[error("ssh server bind or serve: {0}")]
    Serve(#[from] std::io::Error),
}

pub struct SshState<H, C> {
    layout: Layout,
    index: Arc<Index>,
    atproto: Arc<Atproto<H, C>>,
    knot_actor: ActorId,
    events: Arc<EventLog<C>>,
    hostname: KnotHostname,
    appview: AppviewEndpoint,
    admins: BTreeSet<AccountDid>,
    admission: AdmissionPolicy,
    limits: PackLimits,
    max_pack_bytes: MaxWireBytes,
    archive_limit: ArchiveLimit,
    languages_push_budget: LanguagesPushBudget,
    ci_logs: Option<CiLogsAddr>,
    slots: Slots,
    lookup_slots: ResolveSlots,
    lookup_peers: Arc<PreAuthLimiter>,
    probe_pace: SubjectPacer,
    miss_pace: PeerPacer,
    key_ttl: KeyTtl,
    peer_slots: Arc<PreAuthLimiter>,
    maintenance: MaintenanceHandle,
    lfs: Option<LfsRuntime>,
    catalog: Arc<knot_messages::Catalog>,
}

#[derive(Clone)]
pub(crate) struct LfsRuntime {
    pub(crate) handle: knot_lfs::LfsHandle,
    pub(crate) slots: Arc<Semaphore>,
    pub(crate) peer_slots: Arc<PreAuthLimiter>,
}

pub struct SshConfig<H, C> {
    pub layout: Layout,
    pub index: Arc<Index>,
    pub atproto: Arc<Atproto<H, C>>,
    pub knot_actor: ActorId,
    pub events: Arc<EventLog<C>>,
    pub hostname: KnotHostname,
    pub appview: AppviewEndpoint,
    pub admins: BTreeSet<AccountDid>,
    pub admission: AdmissionPolicy,
    pub max_pack_bytes: MaxWireBytes,
    pub archive_limit: ArchiveLimit,
    pub languages_push_budget: LanguagesPushBudget,
    pub ci_logs: Option<CiLogsAddr>,
}

impl<H: HttpTransport, C: Clock> SshState<H, C> {
    pub fn new(config: SshConfig<H, C>) -> Self {
        let SshConfig {
            layout,
            index,
            atproto,
            knot_actor,
            events,
            hostname,
            appview,
            admins,
            admission,
            max_pack_bytes,
            archive_limit,
            languages_push_budget,
            ci_logs,
        } = config;
        Self {
            layout,
            index,
            atproto,
            knot_actor,
            events,
            hostname,
            appview,
            admins,
            admission,
            limits: PackLimits::default(),
            max_pack_bytes,
            archive_limit,
            languages_push_budget,
            ci_logs,
            slots: Slots::for_machine(),
            lookup_slots: ResolveSlots::new(MAX_INFLIGHT_LOOKUPS),
            lookup_peers: Arc::new(PreAuthLimiter::with_config(LimitConfig {
                rate: Some(RateLimit {
                    burst: Burst::new(LOOKUP_BURST_PER_PEER),
                    refill: RefillMicros::new(LOOKUP_REFILL_MICROS),
                }),
                per_peer_inflight: Some(PerPeerInflight::new(LOOKUP_INFLIGHT_PER_PEER)),
                global_inflight: Some(GlobalInflight::new(MAX_PREAUTH_LOOKUPS)),
            })),
            probe_pace: SubjectPacer::new(RateLimit {
                burst: Burst::new(PROBE_BURST_PER_ACCOUNT),
                refill: RefillMicros::new(PROBE_REFILL_MICROS),
            }),
            miss_pace: PeerPacer::new(RateLimit {
                burst: Burst::new(MISS_BURST_PER_PEER),
                refill: RefillMicros::new(MISS_REFILL_MICROS),
            }),
            key_ttl: KeyTtl::DEFAULT,
            peer_slots: Arc::new(PreAuthLimiter::with_config(LimitConfig::per_peer_only(
                PerPeerInflight::new(MAX_INFLIGHT_PER_PEER),
            ))),
            maintenance: MaintenanceHandle::disabled(),
            lfs: None,
            catalog: Arc::new(knot_messages::Catalog::defaults()),
        }
    }

    pub fn with_catalog(mut self, catalog: Arc<knot_messages::Catalog>) -> Self {
        self.catalog = catalog;
        self
    }

    pub fn with_slots(mut self, slots: Slots) -> Self {
        self.slots = slots;
        self
    }

    pub fn with_key_ttl(mut self, ttl: KeyTtl) -> Self {
        self.key_ttl = ttl;
        self
    }

    pub fn with_maintenance(mut self, maintenance: MaintenanceHandle) -> Self {
        self.maintenance = maintenance;
        self
    }

    pub fn with_lfs(mut self, handle: knot_lfs::LfsHandle, max_transfers: usize) -> Self {
        self.lfs = Some(LfsRuntime {
            handle,
            slots: Arc::new(Semaphore::new(max_transfers)),
            peer_slots: Arc::new(PreAuthLimiter::with_config(LimitConfig::per_peer_only(
                PerPeerInflight::new(max_transfers),
            ))),
        });
        self
    }

    pub fn with_limits(mut self, limits: PackLimits) -> Self {
        self.limits = limits;
        self
    }
}

fn server_config(host_key: PrivateKey) -> Arc<Config> {
    Arc::new(Config {
        keys: vec![host_key],
        methods: MethodSet::from(&[MethodKind::PublicKey][..]),
        inactivity_timeout: Some(INACTIVITY_TIMEOUT),
        keepalive_interval: Some(KEEPALIVE_INTERVAL),
        auth_rejection_time: AUTH_REJECTION_TIME,
        preferred: Preferred {
            compression: Cow::Borrowed(&[compression::NONE]),
            ..Preferred::DEFAULT
        },
        ..Config::default()
    })
}

pub async fn serve<H: HttpTransport, C: Clock>(
    addr: SocketAddr,
    host_key: PrivateKey,
    state: Arc<SshState<H, C>>,
    shutdown: CancellationToken,
) -> Result<(), SshError> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_drained(listener, host_key, state, shutdown).await
}

pub async fn serve_on_socket<H: HttpTransport, C: Clock>(
    listener: tokio::net::TcpListener,
    host_key: PrivateKey,
    state: Arc<SshState<H, C>>,
) -> Result<(), SshError> {
    serve_drained(listener, host_key, state, CancellationToken::new()).await
}

#[doc(hidden)]
pub async fn serve_drained<H: HttpTransport, C: Clock>(
    listener: tokio::net::TcpListener,
    host_key: PrivateKey,
    state: Arc<SshState<H, C>>,
    shutdown: CancellationToken,
) -> Result<(), SshError> {
    let config = server_config(host_key);
    let tracker = TaskTracker::new();
    let mut server = KnotSshServer {
        state,
        tracker: tracker.clone(),
    };
    loop {
        let accepted = tokio::select! {
            () = shutdown.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(pair) => pair,
            Err(error) if is_connection_error(&error) => continue,
            Err(error) => {
                tracing::warn!("ssh accept failed, backing off: {error}");
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    () = tokio::time::sleep(ACCEPT_BACKOFF) => {}
                }
                continue;
            }
        };
        let handler = server.new_client(Some(peer));
        let config = Arc::clone(&config);
        tracker.spawn(async move {
            if let Ok(session) = russh::server::run_stream(config, stream, handler).await {
                let _ = session.await;
            }
        });
    }
    tracker.close();
    let _ = tokio::time::timeout(DRAIN_GRACE, tracker.wait()).await;
    Ok(())
}

fn is_connection_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
    )
}

pub fn load_or_create_host_key(path: &Path) -> Result<PrivateKey, SshError> {
    let report = |message: String| SshError::HostKey {
        path: path.display().to_string(),
        message,
    };
    let load = || {
        ensure_secure_perms(path).map_err(&report)?;
        russh::keys::load_secret_key(path, None).map_err(|error| report(error.to_string()))
    };
    if path.exists() {
        return load();
    }
    let key = PrivateKey::random(&mut EntropyRng, Algorithm::Ed25519)
        .map_err(|error| report(error.to_string()))?;
    match persist_host_key(path, &key)? {
        Claim::Won => Ok(key),
        Claim::Lost => load(),
    }
}

enum Claim {
    Won,
    Lost,
}

fn persist_host_key(path: &Path, key: &PrivateKey) -> Result<Claim, SshError> {
    let report = |message: String| SshError::HostKey {
        path: path.display().to_string(),
        message,
    };
    let pem = key
        .to_openssh(ssh_key::LineEnding::LF)
        .map_err(|error| report(error.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| report(error.to_string()))?;
    }
    let temp = unique_temp(path);
    write_secret(&temp, pem.as_bytes()).map_err(|error| report(error.to_string()))?;
    // `hard_link` instead of `rename` so that in case two knots
    // are booting at the same time they don't get borked
    // if one clobber's the other's key. here the loser reloads
    // winner's key.
    let claim = match std::fs::hard_link(&temp, path) {
        Ok(()) => Claim::Won,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Claim::Lost,
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            return Err(report(error.to_string()));
        }
    };
    let _ = std::fs::remove_file(&temp);
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(claim)
}

#[cfg(unix)]
fn ensure_secure_perms(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let mode = std::fs::metadata(path)
        .map_err(|error| error.to_string())?
        .mode();
    if mode & 0o077 != 0 {
        return Err(format!(
            "private host key is group or other accessible at mode {:o}, run chmod 600 on it",
            mode & 0o777
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_secure_perms(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn unique_temp(path: &Path) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
    let stem = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("ssh_host_key");
    path.with_file_name(format!(".{stem}.{}.{nonce}.tmp", std::process::id()))
}

fn write_secret(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

struct EntropyRng;

impl rand_core::TryRng for EntropyRng {
    type Error = std::convert::Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        let mut bytes = [0u8; 4];
        OsEntropy.fill(&mut bytes);
        Ok(u32::from_le_bytes(bytes))
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        let mut bytes = [0u8; 8];
        OsEntropy.fill(&mut bytes);
        Ok(u64::from_le_bytes(bytes))
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
        OsEntropy.fill(dst);
        Ok(())
    }
}

impl rand_core::TryCryptoRng for EntropyRng {}
