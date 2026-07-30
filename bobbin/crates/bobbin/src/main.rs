use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow};
use bobbin_edge_index::{CoverageWatch, EdgeStore, HydrantCursor, StateIndex};
use bobbin_ingest::{
    IngestConfig, IngestRuntime, RepoIdResolver, WarmingBuffer, run as run_ingest,
};
use bobbin_knot_ingest::{CapabilityGate, KnotClient, KnotRegistry, Orchestrator};
use bobbin_knot_proxy::{KnotHttpConfig, KnotProxy, KnotProxyConfig, classify_ip};
use bobbin_record_lru::{CacheCapacity, LruRecordStore, RecordStore};
use bobbin_runtime::{
    Clock, GuardedWs, MemoryBudget, NetworkError, OsEntropy, RuntimeHasher, SystemClock,
    TungsteniteWs, WsTransport,
};
use bobbin_search::{SearchIndex, SearchReader};
use bobbin_slingshot_client::SlingshotClient;
use bobbin_xrpc::{
    AppState, HeavyLimiter, MaxInFlight, PerRequestAnonBytes, ReservedFloor, router,
};
use clap::{Parser, Subcommand};
use tokio::signal::unix::{SignalKind, signal};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;

mod config;
mod mem;

use config::{BobbinConfig, LogFormat};

const BASE_RESIDENT_BYTES: u64 = 48 * 1024 * 1024;

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[used]
#[unsafe(no_mangle)]
pub static malloc_conf: Option<&'static core::ffi::c_char> = Some(unsafe {
    &*c"narenas:8,dirty_decay_ms:3000,muzzy_decay_ms:3000,background_thread:true".as_ptr()
});

#[derive(Parser)]
#[command(name = "bobbin", about = "Read-only AppView for Tangled records")]
struct Cli {
    /// Path to a TOML config file. Environment variables override file values
    /// for any `BOBBIN_*` setting; `/etc/bobbin/config.toml` is consulted as a
    /// final fallback so distro packaging can drop a default in place.
    #[arg(short, long, value_name = "FILE", env = "BOBBIN_CONFIG")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print a fully-commented TOML template to stdout. Use this to seed
    /// `config.toml` for a fresh deploy.
    ConfigTemplate,
    /// Load and validate the configuration without starting the server.
    Validate,
}

#[tokio::main]
async fn main() -> ExitCode {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cli = Cli::parse();

    if let Some(Command::ConfigTemplate) = cli.command {
        print!("{}", config::template());
        return ExitCode::SUCCESS;
    }

    let cfg = match config::load(cli.config.as_ref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to load configuration: {e:#}");
            return ExitCode::FAILURE;
        }
    };

    if matches!(cli.command, Some(Command::Validate)) {
        if let Err(e) = init_tracing(&cfg, Arc::new(SystemClock::new())) {
            eprintln!("failed to install tracing subscriber: {e}");
            return ExitCode::FAILURE;
        }
        println!("configuration is valid");
        return ExitCode::SUCCESS;
    }

    match run(cfg).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = ?e, "fatal");
            ExitCode::FAILURE
        }
    }
}

struct ClockTimer(Arc<dyn Clock>);

impl tracing_subscriber::fmt::time::FormatTime for ClockTimer {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", self.0.now_unix_micros().raw())
    }
}

fn init_tracing(cfg: &BobbinConfig, clock: Arc<dyn Clock>) -> Result<(), String> {
    let combined = format!("{},{}", LevelFilter::INFO, cfg.log.filter);
    let format: LogFormat = cfg.log.format.parse()?;
    let timer = ClockTimer(clock);
    match format {
        LogFormat::Text => {
            let filter = EnvFilter::try_new(&combined)
                .map_err(|e| format!("invalid log filter `{}`: {e}", cfg.log.filter))?;
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_timer(timer)
                .try_init()
                .map_err(|e| e.to_string())
        }
        LogFormat::Json => {
            let filter = EnvFilter::try_new(&combined)
                .map_err(|e| format!("invalid log filter `{}`: {e}", cfg.log.filter))?;
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .json()
                .with_timer(timer)
                .try_init()
                .map_err(|e| e.to_string())
        }
    }
}

async fn run(cfg: BobbinConfig) -> anyhow::Result<()> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    init_tracing(&cfg, clock.clone())
        .map_err(|e| anyhow!("failed to install tracing subscriber: {e}"))?;

    let (budget, budget_source) = mem::detect_budget();
    tracing::info!(
        source = ?budget_source,
        budget_bytes = budget.map(MemoryBudget::bytes),
        "memory budget detected"
    );
    if let Some(b) = budget {
        mem::try_set_high(b);
    }
    let lru_cap = mem::lru_bytes(budget, cfg.record_cache.lru_bytes);
    let search_heap_cap = mem::search_heap_bytes(budget, cfg.search.heap_bytes);
    if budget.is_some() {
        tracing::info!(
            lru_bytes = lru_cap,
            search_heap_bytes = search_heap_cap,
            "constrained cache sizing"
        );
    }
    let limiter = budget.map(|b| {
        let reserved = ReservedFloor::new(
            BASE_RESIDENT_BYTES
                .saturating_add(cfg.backpressure.reserved_index_bytes)
                .saturating_add(search_heap_cap)
                .saturating_add(lru_cap),
        );
        let per_request = PerRequestAnonBytes::new(cfg.backpressure.per_request_anon_bytes);
        let max = MaxInFlight::from_budget(b, reserved, per_request);
        tracing::info!(max_in_flight = max.get(), "heavy-request concurrency cap");
        Arc::new(HeavyLimiter::new(max))
    });

    let entropy = Arc::new(OsEntropy);
    let hasher = RuntimeHasher::from_entropy(&*entropy);
    let ws = TungsteniteWs::shared();

    let records: Arc<dyn RecordStore> =
        Arc::new(LruRecordStore::new(CacheCapacity::from_bytes(lru_cap)));
    let slingshot = SlingshotClient::with_default_http(cfg.slingshot.url.clone())?;
    let resolver = Arc::new(RepoIdResolver::with_slingshot(
        slingshot.clone(),
        clock.clone(),
        hasher.clone(),
    ));
    let edges = Arc::new(EdgeStore::new(hasher.clone()));
    let issue_states = Arc::new(StateIndex::new(hasher.clone()));
    let pull_statuses = Arc::new(StateIndex::new(hasher.clone()));
    let coverage = Arc::new(CoverageWatch::new());
    let warming_buffer = Arc::new(WarmingBuffer::new(hasher.clone()));
    let knot_registry = Arc::new(KnotRegistry::new());
    let knots = Arc::new(KnotProxy::new(
        KnotProxyConfig {
            allow_private_hosts: cfg.knot.allow_private,
            require_https: cfg.knot.require_https,
            ..KnotProxyConfig::default()
        },
        KnotHttpConfig::default(),
        clock.clone(),
        hasher,
    )?);
    let search_heap = usize::try_from(search_heap_cap)
        .with_context(|| format!("search heap {search_heap_cap} exceeds usize"))?;
    let search = Arc::new(SearchIndex::new(search_heap, clock.clone())?);

    let configured_parallelism = NonZeroUsize::new(cfg.ingest.parallelism)
        .ok_or_else(|| anyhow!("ingest.parallelism must be at least 1"))?;
    let parallelism = mem::ingest_parallelism(budget, configured_parallelism);
    tracing::info!(
        configured = configured_parallelism.get(),
        effective = parallelism.get(),
        "ingest parallelism"
    );
    let ingest_cfg = IngestConfig {
        hydrant_base: cfg.hydrant.url.clone(),
        start_cursor: HydrantCursor::new(cfg.hydrant.start_cursor),
        parallelism,
    };
    let cancel = CancellationToken::new();
    let ingest_coverage = coverage.clone();

    let knot_acl_dev = !cfg.knot.require_https;
    let knot_allow_private = cfg.knot.allow_private;
    let knot_client = KnotClient::with_default_http(knot_allow_private)?;
    let knot_gate = Arc::new(CapabilityGate::new(
        knot_client.clone(),
        clock.clone(),
        knot_acl_dev,
        knot_allow_private,
    ));
    let knot_ws: Arc<dyn WsTransport> = if knot_allow_private {
        ws.clone()
    } else {
        GuardedWs::shared(Arc::new(|addrs: &[SocketAddr]| {
            match addrs.iter().find_map(|sa| classify_ip(&sa.ip())) {
                Some(reason) => Err(NetworkError::Connect(format!(
                    "knot eventstream resolves to {reason} address space"
                ))),
                None => Ok(()),
            }
        }))
    };

    let ingest_runtime = IngestRuntime {
        store: edges.clone(),
        issue_states: issue_states.clone(),
        pull_statuses: pull_statuses.clone(),
        coverage: coverage.clone(),
        search: search.clone(),
        records: records.clone(),
        resolver: resolver.clone(),
        clock: clock.clone(),
        entropy,
        ws: ws.clone(),
        cancel: cancel.clone(),
        disconnects: None,
        warming_shadow: None,
        warming_buffer: Some(warming_buffer),
        knot_registry: Some(knot_registry.clone()),
        knot_gate: Some(knot_gate.clone()),
    };
    let mut ingest_handle = tokio::spawn(run_ingest(ingest_cfg, ingest_runtime));

    let knot_orchestrator = Orchestrator {
        client: Arc::new(knot_client),
        gate: knot_gate,
        registry: knot_registry,
        store: edges.clone(),
        ws: knot_ws,
        clock: clock.clone(),
        dev: knot_acl_dev,
        allow_private: knot_allow_private,
        cancel: cancel.clone(),
    };
    let _knot_acl_handle = tokio::spawn(knot_orchestrator.run());

    let _adaptive_watcher = budget.zip(limiter.as_ref()).map(|(b, l)| {
        mem::spawn_adaptive_watcher(
            l.clone(),
            clock.clone(),
            b,
            mem::AdaptiveThresholds {
                interval: Duration::from_millis(cfg.backpressure.adjust_interval_ms),
                relieve_below_ratio: cfg.backpressure.relieve_below_ratio,
                tighten_above_ratio: cfg.backpressure.tighten_above_ratio,
            },
            cancel.clone(),
        )
    });

    let debug_bind: Option<SocketAddr> =
        if cfg.server.debug_bind.is_empty() {
            None
        } else {
            Some(cfg.server.debug_bind.parse().with_context(|| {
                format!("invalid server.debug_bind `{}`", cfg.server.debug_bind)
            })?)
        };
    let mem_probe = debug_bind.is_some().then(|| mem::MemProbe {
        edges: edges.clone(),
        search: search.clone(),
        records: records.clone(),
        issue_states: issue_states.clone(),
        pull_statuses: pull_statuses.clone(),
    });
    let state = AppState::new(
        records,
        slingshot,
        edges,
        issue_states,
        pull_statuses,
        coverage,
        knots,
        search as Arc<dyn SearchReader>,
        resolver,
    )
    .with_limiter(limiter);
    let app = router(state);

    let _debug_server = match (debug_bind, mem_probe) {
        (Some(addr), Some(probe)) => {
            let debug_app = mem::debug_router(probe);
            let debug_cancel = cancel.clone();
            Some(tokio::spawn(async move {
                match bind_listener(addr) {
                    Ok(listener) => {
                        tracing::info!(%addr, "debug endpoints bound, keep this loopback-only");
                        let shutdown = async move { debug_cancel.cancelled().await };
                        if let Err(e) = axum::serve(listener, debug_app)
                            .with_graceful_shutdown(shutdown)
                            .await
                        {
                            tracing::warn!(error = %e, "debug endpoint server exited with error");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(%addr, error = %e, "could not bind debug endpoints");
                    }
                }
            }))
        }
        _ => None,
    };

    let binds = cfg.server.binds.clone();
    let hydrant_url = cfg.hydrant.url.as_str().to_owned();
    let slingshot_url = cfg.slingshot.url.as_str().to_owned();
    let grace = Duration::from_secs(cfg.server.shutdown_grace_secs);
    let bind_display = binds
        .iter()
        .map(SocketAddr::to_string)
        .collect::<Vec<_>>()
        .join(",");
    tracing::info!(binds = %bind_display, %hydrant_url, %slingshot_url, "bobbin listening");

    let signal_cancel = cancel.clone();
    let mut server_handle = tokio::spawn(serve_all(binds, app, signal_cancel));

    tokio::select! {
        res = &mut server_handle => {
            cancel.cancel();
            let cursor = ingest_coverage.snapshot().last_cursor().raw();
            tracing::info!(grace_secs = grace.as_secs(), cursor, "draining ingest");
            drain_with_grace("ingest", grace, &mut ingest_handle, clock.as_ref()).await;
            match res {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(anyhow::Error::from(e)).context("axum server failed"),
                Err(join) => Err(anyhow!("server task panicked: {join}")),
            }
        }
        res = &mut ingest_handle => {
            cancel.cancel();
            let cursor = ingest_coverage.snapshot().last_cursor().raw();
            tracing::info!(grace_secs = grace.as_secs(), cursor, "draining server");
            drain_with_grace("server", grace, &mut server_handle, clock.as_ref()).await;
            match res {
                Ok(Ok(())) => Err(anyhow!("ingest run loop exited; loop is supposed to be infinite")),
                Ok(Err(e)) => Err(anyhow::Error::from(e)).context("ingest exited"),
                Err(join) => Err(anyhow!("ingest task panicked: {join}")),
            }
        }
    }
}

async fn drain_with_grace<T>(
    label: &'static str,
    grace: Duration,
    handle: &mut JoinHandle<T>,
    clock: &dyn Clock,
) {
    tokio::select! {
        _ = &mut *handle => {}
        _ = clock.sleep(grace) => {
            tracing::warn!(
                grace_secs = grace.as_secs(),
                label,
                "task did not stop within grace, aborting"
            );
            handle.abort();
        }
    }
}

async fn serve_all(
    binds: Vec<SocketAddr>,
    app: axum::Router,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let listeners = futures::future::try_join_all(binds.into_iter().map(|addr| async move {
        let listener = bind_listener(addr)?;
        tracing::info!(%addr, "bobbin listener bound");
        Ok::<_, std::io::Error>(listener)
    }))
    .await?;

    let trigger = cancel.clone();
    let signal_task = tokio::spawn(async move {
        wait_for_shutdown().await;
        tracing::info!("shutdown signal received, draining server");
        trigger.cancel();
    });

    let services = listeners.into_iter().map(|listener| {
        let app = app.clone();
        let cancel = cancel.clone();
        async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { cancel.cancelled().await })
                .await
        }
    });

    let result = futures::future::try_join_all(services).await.map(|_| ());
    signal_task.abort();
    result
}

fn bind_listener(addr: SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    let domain = match addr {
        SocketAddr::V4(_) => socket2::Domain::IPV4,
        SocketAddr::V6(_) => socket2::Domain::IPV6,
    };
    let socket = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    if matches!(addr, SocketAddr::V6(_)) {
        socket.set_only_v6(true)?;
    }
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    let std_listener: std::net::TcpListener = socket.into();
    tokio::net::TcpListener::from_std(std_listener)
}

async fn wait_for_shutdown() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                ?e,
                "could not install SIGTERM handler, shutdown will only honor ctrl-c"
            );
            ctrl_c.await.ok();
            return;
        }
    };
    tokio::select! {
        _ = ctrl_c => {}
        _ = sigterm.recv() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn drain_returns_immediately_when_task_already_done() {
        let mut handle = tokio::spawn(async { 7u32 });
        tokio::time::advance(Duration::from_millis(1)).await;
        let start = tokio::time::Instant::now();
        drain_with_grace(
            "test",
            Duration::from_secs(60),
            &mut handle,
            &SystemClock::new(),
        )
        .await;
        assert!(start.elapsed() < Duration::from_millis(10));
    }

    #[tokio::test(start_paused = true)]
    async fn drain_aborts_runaway_task_after_grace() {
        let mut handle = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        let grace = Duration::from_secs(5);
        let start = tokio::time::Instant::now();
        drain_with_grace("test", grace, &mut handle, &SystemClock::new()).await;
        assert!(start.elapsed() >= grace);
        let outcome = handle.await;
        assert!(outcome.is_err() && outcome.unwrap_err().is_cancelled());
    }

    #[test]
    fn typo_filter_keeps_info_default_for_other_targets() {
        let filter = format!("{},{}", LevelFilter::INFO, "blah_invalid");
        let parsed = EnvFilter::try_new(&filter).expect("filter parses");
        let rendered = parsed.to_string();
        assert!(
            rendered.contains("info"),
            "expected info default, got {rendered}"
        );
        assert!(
            rendered.contains("blah_invalid"),
            "expected user override, got {rendered}",
        );
    }

    #[test]
    fn explicit_user_level_overrides_info_default() {
        let filter = format!("{},{}", LevelFilter::INFO, "warn");
        let parsed = EnvFilter::try_new(&filter).expect("filter parses");
        assert_eq!(parsed.to_string(), "warn");
    }
}
