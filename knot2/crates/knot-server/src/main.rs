mod allocator;

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[allow(non_upper_case_globals)]
#[unsafe(export_name = "_rjem_malloc_conf")]
pub static malloc_conf: &[u8] =
    b"background_thread:true,retain:false,dirty_decay_ms:0,muzzy_decay_ms:0\0";

use std::collections::BTreeSet;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use anyhow::Context;
use axum::Json;
use axum::response::Html;
use axum::routing::get;
use base64::Engine;
use knot_atproto::Atproto;
use knot_config::HomepageSource;
use knot_index::Index;
use knot_runtime::{Clock, HttpTransport, OsEntropy, ReqwestHttp, SystemClock};
use knot_secrets::{MasterKey, SealedStore};
use knot_types::{ActorId, AuthorName, BranchName, CiLogsAddr, Email, KnotHostname, ObjectCount};
use knot_xrpc::XrpcState;
use tower_http::services::ServeFile;

const MAINTENANCE_SHUTDOWN_DRAIN: Duration = Duration::from_secs(30);
const EDGE_SHUTDOWN_DRAIN: Duration = Duration::from_secs(40);

const DEFAULT_HOMEPAGE: &str = include_str!("homepage.html");

struct IndexRepos(Arc<Index>);

impl knot_maintenance::RepoSource for IndexRepos {
    fn repos(&self) -> Vec<knot_types::RepoDid> {
        self.0.hosted_repos()
    }

    fn ready_repos(&self) -> Option<Vec<knot_types::RepoDid>> {
        match self.0.coverage().registry {
            knot_index::Coverage::Ready => Some(self.0.hosted_repos()),
            knot_index::Coverage::Warming => None,
        }
    }
}

struct AtprotoHandleResolver<H, C> {
    atproto: Arc<Atproto<H, C>>,
}

impl<H: HttpTransport, C: Clock> knot_pack::HandleResolver for AtprotoHandleResolver<H, C> {
    fn resolve(
        &self,
        handle: knot_types::Handle,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<knot_types::AccountDid>> + Send + '_>,
    > {
        Box::pin(async move { self.atproto.resolve_handle_to_did(&handle).await.ok() })
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
        .context("disable core dumps and ptrace attachment")?;

    if std::env::args().nth(1).as_deref() == Some("config-template") {
        print!("{}", knot_config::template());
        return Ok(());
    }

    init_tracing();

    tracing::info!("!");
    tracing::info!("!");
    tracing::info!("!");
    tracing::info!("> If knot1 was so good then why isn't there a... ( ˶°ㅁ°)");
    tracing::info!("...");
    tracing::info!("Welcome to knot2!");
    tracing::info!("This code was made with love.");
    tracing::info!("Hachapuri is sho tasty, definitely worth a try. Better than pizza tbh.");
    tracing::info!("!");
    tracing::info!("!");
    tracing::info!("!");

    let config_path = std::env::args().nth(1).map(PathBuf::from);
    let config = knot_config::load(config_path.as_deref()).context("load configuration")?;
    config
        .verify_environment()
        .context("verify runtime environment")?;

    let resources = knot_resource::init(knot_resource::Ceilings {
        max_threads: match config.resources.max_threads {
            0 => None,
            n => Some(knot_resource::ThreadCount::new(n as usize)),
        },
        max_memory: match config.resources.max_memory_bytes {
            0 => None,
            n => Some(knot_resource::MemoryBudget::new(n)),
        },
    });
    tracing::info!(
        threads = resources.threads.get(),
        memory_bytes = resources.memory.map(knot_resource::MemoryBudget::get),
        memory_source = ?resources.memory_source,
        memory_high_bytes = resources
            .memory_high_bytes
            .map(knot_resource::MemoryHighBytes::get),
        "resource governor initialized"
    );
    allocator::install();

    // I made the hostname stay aa string in the config,
    // so that confique can layer env-over-file.
    // Here's where it becomes a real type,
    // and the DID + the service url stack on this.
    let hostname = KnotHostname::new(config.server.hostname.clone())
        .context("server.hostname isn't a valid knot hostname")?;
    let knot_did = hostname.knot_did();
    let object_format = config.object_format().context("parse git.object_format")?;
    let default_branch =
        BranchName::new(config.repo.default_branch.as_str()).context("parse default branch")?;
    let layout = knot_git::Layout::new(&config.repo.scan_path)
        .with_default_branch(default_branch)
        .with_object_format(object_format)
        .reserving_meta(&knot_did)
        .context("reserve meta-repo path")?;

    let swept = knot_pack::sweep_incoming(&config.repo.scan_path);
    if swept > 0 {
        tracing::info!(swept, "swept abandoned receive staging directories");
    }

    layout
        .bootstrap_meta(&knot_did)
        .context("bootstrap meta-repo")?;
    let meta_path = layout
        .meta_path(&knot_did)
        .context("resolve meta-repo path")?;

    let index = Arc::new(Index::new(meta_path.clone(), layout.clone()));
    index.rebuild().context("rebuild index from meta-repo")?;
    tracing::info!(coverage = ?index.coverage(), "index ready");

    let warm = Arc::clone(&index);
    tokio::task::spawn_blocking(move || warm.warm_collaborators());

    let http = ReqwestHttp::new(config.http_limits()).context("build outbound HTTP client")?;
    let git_http: Arc<dyn knot_runtime::HttpTransport> = Arc::new(
        ReqwestHttp::new(config.fork_http_limits()).context("build outbound git fetch client")?,
    );
    let atproto = Arc::new(Atproto::new(
        http,
        SystemClock,
        knot_did.clone(),
        knot_atproto::PlcDirectory::new(config.atproto.plc_directory.clone())
            .context("atproto.plc_directory isn't a valid PLC base URL")?,
    ));
    let admins: BTreeSet<_> = config.server.admins.iter().cloned().collect();
    let admission = config.acl.admission;
    let service_owner = config
        .server
        .admins
        .first()
        .cloned()
        .context("at least one admin is configured")?;

    let master_key = MasterKey::new(
        base64::engine::general_purpose::STANDARD
            .decode(
                std::env::var(&config.secrets.master_key_env)
                    .context("read master key from environment")?
                    .trim(),
            )
            .context("decode master key as base64")?,
    )
    .context("master key from environment")?;
    let secrets = Arc::new(
        SealedStore::open(
            &config.secrets.sealed_key_file,
            &master_key,
            Box::new(OsEntropy),
        )
        .context("open sealed key store")?,
    );
    let knot_signing_key = secrets
        .ensure(&knot_did)
        .context("seal knot's own signing key")?;
    let knot_actor = ActorId::from_secp256k1(knot_signing_key.as_bytes());
    let appview_endpoint = config.server.appview_endpoint.clone();
    let knot_service_url = knot_types::KnotServiceUrl::new(format!("https://{hostname}"))
        .context("server.hostname doesn't form a valid knot service URL")?;
    let did_document =
        knot_atproto::knot_did_document(&knot_did, &knot_signing_key, &knot_service_url);

    let http_addr = config.server.listen_addr;
    let listen_limits = knot_edge::ListenLimits::new(
        knot_edge::HeaderTimeout::from_millis(
            NonZeroU64::new(config.server.listen_header_timeout_ms)
                .context("server.listen_header_timeout_ms must be greater than zero")?,
        ),
        knot_edge::IdleTimeout::from_millis(
            NonZeroU64::new(config.server.listen_idle_timeout_ms)
                .context("server.listen_idle_timeout_ms must be greater than zero")?,
        ),
        NonZeroU32::new(config.server.listen_max_connections)
            .context("server.listen_max_connections must be greater than zero")?,
    );
    // A header name that doesn't parse will never match,
    // `effective_peer` falls back to socket,
    // and every request in the world shares
    // the proxy's address + its one ratelimit bucket.
    // So... better to refuse to start.
    let trusted_proxy_header = config
        .xrpc
        .trusted_proxy_header
        .as_deref()
        .map(|header| axum::http::HeaderName::from_bytes(header.as_bytes()))
        .transpose()
        .context("xrpc.trusted_proxy_header isn't a valid HTTP header name")?;
    let edge_guards = knot_edge::EdgeGuards::new(
        knot_edge::RequestsPerSecond::new(
            NonZeroU32::new(config.server.listen_rate_limit_per_second)
                .context("server.listen_rate_limit_per_second must be greater than zero")?,
        ),
        knot_edge::BurstSize::new(
            NonZeroU32::new(config.server.listen_rate_limit_burst)
                .context("server.listen_rate_limit_burst must be greater than zero")?,
        ),
        knot_edge::MaxInflightRequests::new(
            NonZeroU32::new(config.server.listen_max_inflight_requests)
                .context("server.listen_max_inflight_requests must be greater than zero")?,
        ),
        knot_edge::RequestTimeout::from_millis(
            NonZeroU64::new(config.server.listen_request_timeout_ms)
                .context("server.listen_request_timeout_ms must be greater than zero")?,
        ),
        knot_edge::BodyInactivityTimeout::from_millis(
            NonZeroU64::new(config.server.listen_body_timeout_ms)
                .context("server.listen_body_timeout_ms must be greater than zero")?,
        ),
        knot_edge::WriteRequestTimeout::from_millis(
            NonZeroU64::new(config.server.listen_write_request_timeout_ms)
                .context("server.listen_write_request_timeout_ms must be greater than zero")?,
        ),
        trusted_proxy_header.clone(),
    );
    let tls_setup = build_tls_setup(&config, &hostname).context("assemble TLS configuration")?;
    if config.tls.http3 && tls_setup.is_none() {
        tracing::warn!(
            "tls.http3 is set without any TLS certificate, so HTTP/3 won't start. Configure a static cert or ACME to serve h3."
        );
    }
    if tls_setup.is_none() && config.xrpc.trusted_proxy_header.is_none() {
        tracing::warn!(
            "running plaintext behind a reverse proxy without xrpc.trusted_proxy_header. Per-IP rate limiting will key on the proxy socket address, throttling all clients as one. Set xrpc.trusted_proxy_header to the header your proxy appends."
        );
    }
    if config.tls.acme_enabled && http_addr.port() != 443 {
        tracing::warn!(
            listen_port = http_addr.port(),
            "ACME validation over TLS-ALPN-01 needs the certificate authority to reach this host on TCP 443. Map 443 to the listen port if it differs."
        );
    }
    if config.tls.acme_enabled && config.tls.acme_staging {
        tracing::warn!(
            "ACME is using the Let's Encrypt staging directory. Its certificates aren't browser-trusted. Unset tls.acme_staging for real certificates."
        );
    }
    let ssh_addr = config.server.ssh_listen_addr;
    let ssh_max_pack_bytes = config.server.ssh_max_pack_bytes as usize;
    let pack_limits = knot_pack::PackLimits {
        max_objects: ObjectCount::from(config.pack.max_objects),
        max_total_bytes: knot_pack::MaxTotalBytes::new(config.pack.max_total_bytes),
        ..knot_pack::PackLimits::default()
    };
    knot_pack::init_selection_limits(knot_pack::SelectionLimits {
        max_objects: ObjectCount::from(config.pack.selection_max_objects),
        time_budget: Duration::from_secs(config.pack.selection_time_budget_secs),
    });
    let host_key = knot_ssh::load_or_create_host_key(&config.server.ssh_host_key_file)
        .context("load or create SSH host key")?;

    let xrpc_limits = knot_xrpc::LimitConfig {
        rate: Some(knot_xrpc::RateLimit {
            burst: knot_xrpc::Burst::new(config.xrpc.preauth_burst),
            refill: knot_xrpc::RefillMicros::new(
                config.xrpc.preauth_refill_ms.saturating_mul(1_000),
            ),
        }),
        per_peer_inflight: Some(knot_xrpc::PerPeerInflight::new(
            config.xrpc.per_peer_inflight as usize,
        )),
        global_inflight: Some(knot_xrpc::GlobalInflight::new(
            config.xrpc.global_inflight as usize,
        )),
    };
    let byte_limits = knot_xrpc::ByteLimits {
        body: knot_xrpc::BodyLimit::new(config.xrpc.max_body_bytes as usize),
        patch: knot_xrpc::PatchLimit::new(config.xrpc.max_patch_bytes as usize),
        patch_decompressed: knot_xrpc::PatchDecompressedLimit::new(
            config.xrpc.max_patch_decompressed_bytes,
        ),
        response: knot_xrpc::ResponseLimit::new(config.xrpc.max_response_bytes as usize),
        archive: knot_xrpc::ArchiveLimit::new(config.xrpc.max_archive_bytes),
        fork_pack: knot_xrpc::ForkPackLimit::new(config.xrpc.fork_max_pack_bytes),
        pack: knot_xrpc::MaxWireBytes::new(ssh_max_pack_bytes),
    };
    let budgets = knot_xrpc::Budgets {
        tree_last_commit: knot_xrpc::TreeReadBudget::new(knot_xrpc::ReadBudget::Within(
            Duration::from_millis(config.xrpc.tree_last_commit_budget_ms),
        )),
        blob_last_commit: knot_xrpc::BlobReadBudget::new(knot_xrpc::ReadBudget::Within(
            Duration::from_millis(config.xrpc.blob_last_commit_budget_ms),
        )),
        languages: knot_xrpc::LanguagesReadBudget::new(knot_xrpc::ReadBudget::Within(
            Duration::from_millis(config.xrpc.languages_budget_ms),
        )),
        languages_push: knot_xrpc::LanguagesPushBudget::new(Duration::from_millis(
            config.xrpc.languages_push_budget_ms,
        )),
    };
    let committer = knot_xrpc::Committer {
        name: AuthorName::new(config.git.user_name.clone()),
        email: Email::new(config.git.user_email.clone()),
    };
    let reservations = Arc::new(knot_xrpc::Reservations::new(
        knot_xrpc::ReservationTtl::new(config.xrpc.reservation_ttl_secs as i64),
        knot_xrpc::PerActorQuota::new(config.xrpc.per_actor_reservations as usize),
        knot_xrpc::GlobalQuota::new(config.xrpc.max_pending_reservations as usize),
    ));
    let replay_bounds = knot_events::ReplayBounds::new(
        knot_events::ReplayEvents::new(config.xrpc.events_replay_buffer as usize)
            .context("xrpc.events_replay_buffer must be greater than zero")?,
        knot_events::ReplayBytes::new(config.xrpc.events_replay_bytes as usize)
            .context("xrpc.events_replay_bytes must be greater than zero")?,
    );
    let events = Arc::new(knot_events::EventLog::new(SystemClock, replay_bounds));
    let subscriber_gate = Arc::new(knot_events::SubscriberGate::new(
        knot_events::GlobalSubscriberLimit::new(config.xrpc.events_max_subscribers as usize),
        knot_events::PerPeerSubscriberLimit::new(config.xrpc.events_max_per_peer as usize),
    ));

    let maintenance_enabled = config.maintenance.enabled;
    let maintenance_options = knot_maintenance::Options::from_config(&config.maintenance);
    let maintenance_interval = Duration::from_secs(config.maintenance.interval_secs);
    let maintenance_large_push =
        knot_maintenance::PushBytes::new(config.maintenance.large_push_bytes);

    let lfs_handle = config
        .lfs
        .store_path
        .as_ref()
        .map(|path| {
            knot_lfs::LfsHandle::open(
                knot_lfs::LfsStorePath::new(path),
                knot_lfs::LfsSize::new(config.lfs.max_object_bytes),
                knot_lfs::FreeSpaceFloor::new(config.lfs.free_space_floor_bytes),
            )
            .inspect(|_| {
                tracing::info!(store = %path.display(), "git-lfs capability enabled");
            })
        })
        .transpose()
        .context("open LFS object store")?;
    let lfs_max_ssh_transfers = config.lfs.max_ssh_transfers as usize;
    let lfs_max_http_downloads = config.lfs.max_http_downloads as usize;
    let lfs_gc_grace = knot_maintenance::lfs_grace(
        knot_maintenance::GcGrace::from_secs(config.lfs.gc_grace_secs),
        knot_maintenance::ReflogRetention::from_secs(config.maintenance.reflog_expire_secs),
    );
    let lfs_gc_interval =
        knot_maintenance::SweepInterval::new(Duration::from_secs(config.lfs.gc_interval_secs));
    if lfs_handle.is_some() && !maintenance_enabled {
        tracing::warn!(
            "LFS store configured but maintenance is disabled. Unreferenced LFS objects will accumulate with no garbage collection or orphan sweep."
        );
    }

    let pack_cache_config = knot_pack::CacheConfig {
        enabled: config.pack_cache.enabled,
        ttl: Duration::from_secs(config.pack_cache.ttl_secs),
        max_entry_bytes: knot_pack::MaxEntryBytes::new(config.pack_cache.max_entry_bytes as usize),
        max_total_bytes: knot_pack::MaxCacheBytes::new(knot_resource::pack_cache_bytes(
            config.pack_cache.max_total_bytes,
        ) as usize),
    };

    let ci_logs = config
        .ci
        .logs_addr
        .as_deref()
        .map(CiLogsAddr::new)
        .transpose()
        .context("ci.logs_addr must be host:port")?;

    let homepage = config.homepage.source();
    let catalog = Arc::new(
        knot_messages::Catalog::parse(&config.messages).context("parse message templates")?,
    );

    knot_config::init(config);

    let (maintenance_handle, maintenance_shutdown, maintenance_task) = if maintenance_enabled {
        let (scheduler, handle) = knot_maintenance::Scheduler::new(
            layout.clone(),
            Arc::new(IndexRepos(Arc::clone(&index))),
            SystemClock,
            maintenance_options,
            maintenance_interval,
            maintenance_large_push,
        );
        let scheduler = match &lfs_handle {
            Some(lfs) => {
                scheduler.with_lfs_gc(Arc::clone(&lfs.store), lfs_gc_grace, lfs_gc_interval)
            }
            None => scheduler,
        };
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(scheduler.run(shutdown_rx));
        tracing::info!(
            interval_secs = maintenance_interval.as_secs(),
            "maintenance scheduler running"
        );
        (handle, Some(shutdown_tx), Some(task))
    } else {
        (knot_maintenance::MaintenanceHandle::disabled(), None, None)
    };

    let slots = knot_resource::Slots::for_machine();

    let ssh_base = knot_ssh::SshState::new(
        layout.clone(),
        Arc::clone(&index),
        Arc::clone(&atproto),
        knot_actor,
        Arc::clone(&events),
        hostname.clone(),
        appview_endpoint.clone(),
        admins.clone(),
        admission,
        byte_limits.pack,
        budgets.languages_push,
        ci_logs.clone(),
    )
    .with_maintenance(maintenance_handle.clone())
    .with_limits(pack_limits)
    .with_slots(slots.clone())
    .with_catalog(Arc::clone(&catalog));
    let ssh_state = Arc::new(match &lfs_handle {
        Some(handle) => ssh_base.with_lfs(handle.clone(), lfs_max_ssh_transfers),
        None => ssh_base,
    });

    let xrpc_state = Arc::new(XrpcState {
        layout: layout.clone(),
        index: Arc::clone(&index),
        atproto: Arc::clone(&atproto),
        secrets,
        entropy: Arc::new(OsEntropy),
        ci_logs,
        admins,
        admission,
        knot_did,
        knot_hostname: hostname,
        meta_path,
        knot_service_url,
        limiter: Arc::new(knot_xrpc::PreAuthLimiter::with_config(xrpc_limits)),
        cob_locks: Arc::new(knot_xrpc::CobLocks::default()),
        reservations,
        trusted_proxy_header,
        committer,
        byte_limits,
        budgets,
        git_http,
        pack_limits,
        service_owner,
        subscriber_gate,
        maintenance: maintenance_handle,
        appview: appview_endpoint,
        slots: slots.clone(),
        events,
        lfs: lfs_handle.map(|handle| knot_xrpc::LfsWeb::new(handle, lfs_max_http_downloads)),
        catalog: Arc::clone(&catalog),
    });

    let resolver: Arc<dyn knot_pack::RepoResolver> = {
        let index = Arc::clone(&index);
        Arc::new(move |target: &knot_pack::RepoTarget| match target {
            knot_pack::RepoTarget::Did(did) => {
                knot_pack::RepoLookup::from_resolved(index.owner_of(did), |_| did.clone())
            }
            knot_pack::RepoTarget::OwnerRkey(owner, rkey) => {
                knot_pack::RepoLookup::from_resolved(index.resolve_repo(owner, rkey), |found| found)
            }
        })
    };
    let receive_advertiser = knot_xrpc::receive_advertiser(Arc::clone(&xrpc_state));
    let handle_resolver: Arc<dyn knot_pack::HandleResolver> = Arc::new(AtprotoHandleResolver {
        atproto: Arc::clone(&atproto),
    });
    let (write_routes, early_data_safe) = knot_pack::edge_routes(
        layout,
        resolver,
        Some(receive_advertiser),
        Some(handle_resolver),
        slots.pack.clone(),
        pack_cache_config,
        Arc::clone(&catalog),
        xrpc_state.knot_hostname.clone(),
        Arc::new(SystemClock),
    );
    let base_router = write_routes.merge(knot_xrpc::router(xrpc_state)).route(
        "/.well-known/did.json",
        get(move || {
            let document = did_document.clone();
            async move { Json(document) }
        }),
    );
    let base_router = match homepage {
        HomepageSource::Disabled => base_router,
        HomepageSource::Default => base_router.route("/", get(|| async { Html(DEFAULT_HOMEPAGE) })),
        HomepageSource::File(path) => base_router.route_service("/", ServeFile::new(path)),
    };
    let app = knot_edge::RequiresFullHandshake::new(base_router);
    let scheme = if tls_setup.is_some() { "https" } else { "http" };
    let edge_config = knot_edge::EdgeConfig {
        http_addr: knot_edge::PublicBind::new(http_addr),
        limits: listen_limits,
        guards: edge_guards,
        tls: tls_setup,
    };
    tracing::info!("listening on {scheme}://{http_addr} and ssh://{ssh_addr}");

    let shutdown = CancellationToken::new();
    tokio::spawn(allocator::govern_decay(shutdown.clone()));
    let mut edge_task = tokio::spawn(knot_edge::serve(
        edge_config,
        app,
        early_data_safe,
        shutdown.clone(),
    ));
    let mut ssh_task = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move { knot_ssh::serve(ssh_addr, host_key, ssh_state, shutdown).await })
    };

    let exit = tokio::select! {
        result = &mut edge_task => FirstExit::Edge(result),
        result = &mut ssh_task => FirstExit::Ssh(result),
        () = shutdown_signal() => {
            tracing::info!("shutdown signal received");
            FirstExit::Signal
        }
    };
    shutdown.cancel();
    let drain = async {
        match &exit {
            FirstExit::Edge(_) => {
                let _ = (&mut ssh_task).await;
            }
            FirstExit::Ssh(_) => {
                let _ = (&mut edge_task).await;
            }
            FirstExit::Signal => {
                let _ = (&mut edge_task).await;
                let _ = (&mut ssh_task).await;
            }
        }
    };
    if tokio::time::timeout(EDGE_SHUTDOWN_DRAIN, drain)
        .await
        .is_err()
    {
        tracing::warn!(
            timeout_secs = EDGE_SHUTDOWN_DRAIN.as_secs(),
            "aborting edge drain after timeout"
        );
    }
    if let Some(shutdown) = maintenance_shutdown {
        let _ = shutdown.send(true);
    }
    if let Some(task) = maintenance_task
        && tokio::time::timeout(MAINTENANCE_SHUTDOWN_DRAIN, task)
            .await
            .is_err()
    {
        tracing::warn!(
            timeout_secs = MAINTENANCE_SHUTDOWN_DRAIN.as_secs(),
            "aborting maintenance drain after timeout :3"
        );
    }
    match exit {
        FirstExit::Edge(result) => result
            .context("edge server task panicked")?
            .context("serve edge")?,
        FirstExit::Ssh(result) => result
            .context("ssh server task panicked")?
            .context("serve ssh")?,
        FirstExit::Signal => {}
    }
    Ok(())
}

enum FirstExit {
    Edge(Result<Result<(), knot_edge::EdgeError>, tokio::task::JoinError>),
    Ssh(Result<Result<(), knot_ssh::SshError>, tokio::task::JoinError>),
    Signal,
}

fn build_tls_setup(
    config: &knot_config::KnotConfig,
    hostname: &knot_types::KnotHostname,
) -> anyhow::Result<Option<knot_edge::TlsSetup>> {
    let tls = &config.tls;
    let source = if tls.acme_enabled {
        knot_edge::CertSource::Acme(knot_edge::AcmeParams {
            domains: vec![hostname.clone()],
            contact: knot_edge::AcmeContact::new(
                tls.acme_contact
                    .clone()
                    .context("tls.acme_contact is required when ACME is enabled")?,
            )?,
            cache_dir: knot_edge::AcmeCacheDir::new(
                tls.acme_cache_dir
                    .clone()
                    .context("tls.acme_cache_dir is required when ACME is enabled")?,
            ),
            production: !tls.acme_staging,
        })
    } else {
        match (&tls.cert_path, &tls.key_path) {
            (Some(cert_path), Some(key_path)) => {
                knot_edge::CertSource::Static(knot_edge::StaticCertPaths {
                    cert_path: knot_edge::CertChainPath::new(cert_path.clone()),
                    key_path: knot_edge::PrivateKeyPath::new(key_path.clone()),
                })
            }
            _ => return Ok(None),
        }
    };

    let internal = match tls.mtls_enabled {
        true => Some(knot_edge::InternalTls {
            addr: knot_edge::InternalBind::new(config.server.internal_listen_addr),
            client_ca_path: knot_edge::ClientCaPath::new(
                tls.mtls_client_ca_path
                    .clone()
                    .context("tls.mtls_client_ca_path is required when mTLS is enabled")?,
            ),
            spki_pin: knot_edge::SpkiPin::from_base64(
                tls.mtls_admin_spki_pin
                    .as_deref()
                    .context("tls.mtls_admin_spki_pin is required when mTLS is enabled")?,
            )
            .context("parse tls.mtls_admin_spki_pin")?,
        }),
        false => None,
    };

    Ok(Some(knot_edge::TlsSetup {
        source,
        http3: tls.http3,
        internal,
    }))
}

async fn shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
}
