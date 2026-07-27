use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use base64::Engine;
use confique::Config;
use knot_runtime::HttpLimits;
use knot_types::{AccountDid, AdmissionPolicy, AppviewEndpoint};
use url::Url;

#[derive(Debug, Config)]
pub struct KnotConfig {
    #[config(nested)]
    pub server: ServerConfig,
    #[config(nested)]
    pub tls: TlsConfig,
    #[config(nested)]
    pub acl: AclConfig,
    #[config(nested)]
    pub repo: RepoConfig,
    #[config(nested)]
    pub git: GitConfig,
    #[config(nested)]
    pub secrets: SecretsConfig,
    #[config(nested)]
    pub http: HttpConfig,
    #[config(nested)]
    pub atproto: AtprotoConfig,
    #[config(nested)]
    pub xrpc: XrpcConfig,
    #[config(nested)]
    pub maintenance: MaintenanceConfig,
    #[config(nested)]
    pub pack_cache: PackCacheConfig,
    #[config(nested)]
    pub pack: PackConfig,
    #[config(nested)]
    pub lfs: LfsConfig,
    #[config(nested)]
    pub resources: ResourcesConfig,
    #[config(nested)]
    pub homepage: HomepageConfig,

    #[config(nested)]
    pub ci: CiConfig,
    #[config(nested)]
    pub messages: knot_messages::MessagesConfig,
}

#[derive(Debug, Config)]
pub struct AclConfig {
    #[config(env = "KNOT_ADMISSION", default = "closed")]
    pub admission: AdmissionPolicy,
}

#[derive(Debug, Config)]
pub struct TlsConfig {
    #[config(env = "KNOT_TLS_CERT_PATH")]
    pub cert_path: Option<PathBuf>,

    #[config(env = "KNOT_TLS_KEY_PATH")]
    pub key_path: Option<PathBuf>,

    #[config(env = "KNOT_TLS_HTTP3", default = true)]
    pub http3: bool,

    #[config(env = "KNOT_TLS_ACME_ENABLED", default = false)]
    pub acme_enabled: bool,

    #[config(env = "KNOT_TLS_ACME_CACHE_DIR")]
    pub acme_cache_dir: Option<PathBuf>,

    #[config(env = "KNOT_TLS_ACME_CONTACT")]
    pub acme_contact: Option<String>,

    #[config(env = "KNOT_TLS_ACME_STAGING", default = false)]
    pub acme_staging: bool,

    #[config(env = "KNOT_TLS_MTLS_ENABLED", default = false)]
    pub mtls_enabled: bool,

    #[config(env = "KNOT_TLS_MTLS_CLIENT_CA_PATH")]
    pub mtls_client_ca_path: Option<PathBuf>,

    #[config(env = "KNOT_TLS_MTLS_ADMIN_SPKI_PIN")]
    pub mtls_admin_spki_pin: Option<String>,
}

#[derive(Debug, Config)]
pub struct ServerConfig {
    #[config(env = "KNOT_HOSTNAME")]
    pub hostname: String,

    #[config(env = "KNOT_ADMINS", parse_env = parse_admins)]
    pub admins: Vec<AccountDid>,

    #[config(env = "KNOT_LISTEN_ADDR", default = "[::]:5555")]
    pub listen_addr: SocketAddr,

    #[config(env = "KNOT_LISTEN_HEADER_TIMEOUT_MS", default = 10_000)]
    pub listen_header_timeout_ms: u64,

    #[config(env = "KNOT_LISTEN_IDLE_TIMEOUT_MS", default = 60_000)]
    pub listen_idle_timeout_ms: u64,

    #[config(env = "KNOT_LISTEN_MAX_CONNECTIONS", default = 1_024)]
    pub listen_max_connections: u32,

    #[config(env = "KNOT_LISTEN_RATE_LIMIT_PER_SECOND", default = 50)]
    pub listen_rate_limit_per_second: u32,

    #[config(env = "KNOT_LISTEN_RATE_LIMIT_BURST", default = 200)]
    pub listen_rate_limit_burst: u32,

    #[config(env = "KNOT_LISTEN_MAX_INFLIGHT_REQUESTS", default = 1_024)]
    pub listen_max_inflight_requests: u32,

    #[config(env = "KNOT_LISTEN_REQUEST_TIMEOUT_MS", default = 60_000)]
    pub listen_request_timeout_ms: u64,

    #[config(env = "KNOT_LISTEN_BODY_TIMEOUT_MS", default = 30_000)]
    pub listen_body_timeout_ms: u64,

    #[config(env = "KNOT_LISTEN_WRITE_REQUEST_TIMEOUT_MS", default = 1_800_000)]
    pub listen_write_request_timeout_ms: u64,

    #[config(env = "KNOT_INTERNAL_LISTEN_ADDR", default = "[::1]:5444")]
    pub internal_listen_addr: SocketAddr,

    #[config(env = "KNOT_SSH_LISTEN_ADDR", default = "[::]:2222")]
    pub ssh_listen_addr: SocketAddr,

    #[config(env = "KNOT_SSH_HOST_KEY_FILE")]
    pub ssh_host_key_file: PathBuf,

    #[config(env = "KNOT_SSH_MAX_PACK_BYTES", default = 8_589_934_592u64)]
    pub ssh_max_pack_bytes: u64,

    #[config(env = "KNOT_APPVIEW_ENDPOINT", default = "https://tangled.org")]
    pub appview_endpoint: AppviewEndpoint,
}

#[derive(Debug, Config)]
pub struct RepoConfig {
    #[config(env = "KNOT_SCAN_PATH")]
    pub scan_path: PathBuf,

    #[config(env = "KNOT_DEFAULT_BRANCH", default = "main")]
    pub default_branch: String,
}

#[derive(Debug, Config)]
pub struct CiConfig {
    #[config(env = "KNOT_CI_LOGS_ADDR")]
    pub logs_addr: Option<String>,
}

#[derive(Debug, Config)]
pub struct HomepageConfig {
    #[config(env = "KNOT_HOMEPAGE_ENABLED", default = true)]
    pub enabled: bool,

    #[config(env = "KNOT_HOMEPAGE_PATH")]
    pub path: Option<PathBuf>,
}

#[derive(Debug)]
pub enum HomepageSource {
    Disabled,
    Default,
    File(PathBuf),
}

impl HomepageConfig {
    pub fn source(&self) -> HomepageSource {
        match (self.enabled, self.path.as_ref()) {
            (false, _) => HomepageSource::Disabled,
            (true, None) => HomepageSource::Default,
            (true, Some(path)) => HomepageSource::File(path.clone()),
        }
    }
}

#[derive(Debug, Config)]
pub struct GitConfig {
    /// Committer identity stamped on merge commits the knot creates.
    #[config(env = "KNOT_GIT_USER_NAME", default = "Tangled")]
    pub user_name: String,

    #[config(env = "KNOT_GIT_USER_EMAIL", default = "noreply@tangled.sh")]
    pub user_email: String,

    #[config(env = "KNOT_GIT_OBJECT_FORMAT", default = "sha256")]
    pub object_format: String,
}

#[derive(Debug, Config)]
pub struct SecretsConfig {
    #[config(env = "KNOT_SEALED_KEY_FILE")]
    pub sealed_key_file: PathBuf,

    #[config(env = "KNOT_MASTER_KEY_ENV")]
    pub master_key_env: String,
}

#[derive(Debug, Config)]
pub struct HttpConfig {
    #[config(env = "KNOT_HTTP_CONNECT_TIMEOUT_MS", default = 5_000)]
    pub connect_timeout_ms: u64,

    #[config(env = "KNOT_HTTP_READ_TIMEOUT_MS", default = 30_000)]
    pub read_timeout_ms: u64,

    #[config(env = "KNOT_HTTP_REQUEST_TIMEOUT_MS", default = 60_000)]
    pub request_timeout_ms: u64,

    #[config(env = "KNOT_HTTP_MAX_RESPONSE_BYTES", default = 16_777_216)]
    pub max_response_bytes: u64,
}

#[derive(Debug, Config)]
pub struct AtprotoConfig {
    #[config(env = "KNOT_PLC_DIRECTORY")]
    pub plc_directory: Url,
}

#[derive(Debug, Config)]
pub struct XrpcConfig {
    #[config(env = "KNOT_XRPC_MAX_BODY_BYTES", default = 65_536)]
    pub max_body_bytes: u64,

    #[config(env = "KNOT_XRPC_MAX_RESPONSE_BYTES", default = 5_242_880)]
    pub max_response_bytes: u64,

    #[config(env = "KNOT_XRPC_MAX_ARCHIVE_BYTES", default = 1_073_741_824)]
    pub max_archive_bytes: u64,

    #[config(env = "KNOT_XRPC_TREE_LAST_COMMIT_BUDGET_MS", default = 300)]
    pub tree_last_commit_budget_ms: u64,

    #[config(env = "KNOT_XRPC_BLOB_LAST_COMMIT_BUDGET_MS", default = 2_000)]
    pub blob_last_commit_budget_ms: u64,

    #[config(env = "KNOT_XRPC_LANGUAGES_BUDGET_MS", default = 1_000)]
    pub languages_budget_ms: u64,

    #[config(env = "KNOT_XRPC_LANGUAGES_PUSH_BUDGET_MS", default = 2_000)]
    pub languages_push_budget_ms: u64,

    /// Body limit for the merge and mergeCheck procedures, whose patch payloads
    /// routinely exceed the general XRPC body limit.
    #[config(env = "KNOT_XRPC_MAX_PATCH_BYTES", default = 16_777_216)]
    pub max_patch_bytes: u64,

    /// Limit on the total decompressed size of a patch the merge procedures parse,
    /// bounding binary-delta inflation and hunk expansion apart from the
    /// compressed body limit above.
    #[config(env = "KNOT_XRPC_MAX_PATCH_DECOMPRESSED_BYTES", default = 134_217_728)]
    pub max_patch_decompressed_bytes: u64,

    #[config(env = "KNOT_XRPC_PREAUTH_BURST", default = 20)]
    pub preauth_burst: u32,

    #[config(env = "KNOT_XRPC_PREAUTH_REFILL_MS", default = 100)]
    pub preauth_refill_ms: u64,

    #[config(env = "KNOT_XRPC_PER_PEER_INFLIGHT", default = 8)]
    pub per_peer_inflight: u32,

    #[config(env = "KNOT_XRPC_GLOBAL_INFLIGHT", default = 64)]
    pub global_inflight: u32,

    #[config(env = "KNOT_XRPC_MAX_PENDING_RESERVATIONS", default = 256)]
    pub max_pending_reservations: u32,

    /// Per-account limit on reserved repository keys awaiting creation, so one
    /// account cannot consume the whole pending-reservation budget.
    #[config(env = "KNOT_XRPC_PER_ACTOR_RESERVATIONS", default = 32)]
    pub per_actor_reservations: u32,

    /// How long a reserved repository key is held before it lapses and its
    /// sealed key is reclaimed, in seconds.
    #[config(env = "KNOT_XRPC_RESERVATION_TTL_SECS", default = 3600)]
    pub reservation_ttl_secs: u64,

    #[config(env = "KNOT_XRPC_FORK_MAX_PACK_BYTES", default = 1_073_741_824)]
    pub fork_max_pack_bytes: u64,

    #[config(env = "KNOT_XRPC_FORK_FETCH_TIMEOUT_MS", default = 600_000)]
    pub fork_fetch_timeout_ms: u64,

    /// When the knot runs behind a trusted reverse proxy that terminates TLS,
    /// set this to the header the proxy appends the client address to, for
    /// example x-forwarded-for. The rightmost entry is used. Leave unset when
    /// the knot is directly exposed so the socket peer address is used. Only set
    /// this when a trusted proxy overwrites or appends the header, since a client
    /// can forge it otherwise.
    #[config(env = "KNOT_XRPC_TRUSTED_PROXY_HEADER")]
    pub trusted_proxy_header: Option<String>,

    #[config(env = "KNOT_XRPC_EVENTS_REPLAY_BUFFER", default = 4096)]
    pub events_replay_buffer: u32,

    #[config(env = "KNOT_XRPC_EVENTS_REPLAY_BYTES", default = 67_108_864)]
    pub events_replay_bytes: u64,

    #[config(env = "KNOT_XRPC_EVENTS_MAX_SUBSCRIBERS", default = 256)]
    pub events_max_subscribers: u32,

    #[config(env = "KNOT_XRPC_EVENTS_MAX_PER_PEER", default = 8)]
    pub events_max_per_peer: u32,
}

#[derive(Debug, Config)]
pub struct MaintenanceConfig {
    #[config(env = "KNOT_MAINTENANCE_ENABLED", default = true)]
    pub enabled: bool,

    #[config(env = "KNOT_MAINTENANCE_COMMIT_GRAPH", default = true)]
    pub commit_graph: bool,

    #[config(env = "KNOT_MAINTENANCE_MULTI_PACK_INDEX", default = true)]
    pub multi_pack_index: bool,

    #[config(env = "KNOT_MAINTENANCE_BITMAP", default = true)]
    pub bitmap: bool,

    #[config(env = "KNOT_MAINTENANCE_INTERVAL_SECS", default = 21_600)]
    pub interval_secs: u64,

    #[config(env = "KNOT_MAINTENANCE_REPACK_MAX_OBJECTS", default = 16_000_000)]
    pub repack_max_objects: u64,

    #[config(env = "KNOT_MAINTENANCE_REPACK_GEOMETRIC_FACTOR", default = 2)]
    pub repack_geometric_factor: u64,

    #[config(env = "KNOT_MAINTENANCE_PRUNE_GRACE_SECS", default = 1_209_600)]
    pub prune_grace_secs: u64,

    #[config(env = "KNOT_MAINTENANCE_REFLOG_EXPIRE_SECS", default = 7_776_000)]
    pub reflog_expire_secs: u64,

    #[config(env = "KNOT_MAINTENANCE_LARGE_PUSH_BYTES", default = 52_428_800)]
    pub large_push_bytes: u64,
}

#[derive(Debug, Config)]
pub struct PackCacheConfig {
    #[config(env = "KNOT_PACK_CACHE_ENABLED", default = true)]
    pub enabled: bool,

    #[config(env = "KNOT_PACK_CACHE_TTL_SECS", default = 60)]
    pub ttl_secs: u64,

    #[config(env = "KNOT_PACK_CACHE_MAX_ENTRY_BYTES", default = 67_108_864)]
    pub max_entry_bytes: u64,

    #[config(env = "KNOT_PACK_CACHE_MAX_TOTAL_BYTES", default = 2_147_483_648u64)]
    pub max_total_bytes: u64,
}

#[derive(Debug, Config)]
pub struct PackConfig {
    #[config(env = "KNOT_PACK_MAX_OBJECTS", default = 16_000_000)]
    pub max_objects: u32,

    #[config(env = "KNOT_PACK_MAX_TOTAL_BYTES", default = 68_719_476_736u64)]
    pub max_total_bytes: u64,

    #[config(env = "KNOT_PACK_SELECTION_MAX_OBJECTS", default = 16_000_000)]
    pub selection_max_objects: u32,

    #[config(env = "KNOT_PACK_SELECTION_TIME_BUDGET_SECS", default = 600)]
    pub selection_time_budget_secs: u64,
}

#[derive(Debug, Config)]
pub struct LfsConfig {
    #[config(env = "KNOT_LFS_STORE_PATH")]
    pub store_path: Option<PathBuf>,

    #[config(env = "KNOT_LFS_MAX_OBJECT_BYTES", default = 5_368_709_120u64)]
    pub max_object_bytes: u64,

    #[config(env = "KNOT_LFS_FREE_SPACE_FLOOR_BYTES", default = 1_073_741_824u64)]
    pub free_space_floor_bytes: u64,

    #[config(env = "KNOT_LFS_GC_GRACE_SECS", default = 1_209_600)]
    pub gc_grace_secs: u64,

    #[config(env = "KNOT_LFS_GC_INTERVAL_SECS", default = 21_600)]
    pub gc_interval_secs: u64,

    #[config(env = "KNOT_LFS_MAX_SSH_TRANSFERS", default = 16)]
    pub max_ssh_transfers: u32,

    #[config(env = "KNOT_LFS_MAX_HTTP_DOWNLOADS", default = 64)]
    pub max_http_downloads: u32,
}

#[derive(Debug, Config)]
pub struct ResourcesConfig {
    #[config(env = "KNOT_MAX_THREADS", default = 0)]
    pub max_threads: u32,

    #[config(env = "KNOT_MAX_MEMORY_BYTES", default = 0)]
    pub max_memory_bytes: u64,
}

fn parse_admins(raw: &str) -> Result<Vec<AccountDid>, knot_types::ParseError> {
    raw.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(AccountDid::new)
        .collect()
}

impl KnotConfig {
    pub fn object_format(&self) -> Option<knot_types::ObjectFormat> {
        knot_types::ObjectFormat::from_capability(&self.git.object_format)
    }

    pub fn tls_enabled(&self) -> bool {
        self.static_cert_enabled() || self.tls.acme_enabled
    }

    pub fn static_cert_enabled(&self) -> bool {
        self.tls.cert_path.is_some() && self.tls.key_path.is_some()
    }

    pub fn http_limits(&self) -> HttpLimits {
        HttpLimits {
            connect_timeout: Duration::from_millis(self.http.connect_timeout_ms),
            read_timeout: Duration::from_millis(self.http.read_timeout_ms),
            request_timeout: Duration::from_millis(self.http.request_timeout_ms),
            max_response_bytes: self.http.max_response_bytes,
            block_private_addresses: true,
        }
    }

    pub fn fork_http_limits(&self) -> HttpLimits {
        HttpLimits {
            connect_timeout: Duration::from_millis(self.http.connect_timeout_ms),
            read_timeout: Duration::from_millis(self.http.read_timeout_ms),
            request_timeout: Duration::from_millis(self.xrpc.fork_fetch_timeout_ms),
            max_response_bytes: self
                .xrpc
                .fork_max_pack_bytes
                .saturating_add(self.xrpc.fork_max_pack_bytes / 64)
                .saturating_add(1_048_576),
            block_private_addresses: true,
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let errors: Vec<String> = [
            check(
                !self.server.hostname.is_empty(),
                "server.hostname mustn't be empty",
            ),
            knot_messages::Catalog::parse(&self.messages)
                .err()
                .map(|error| error.to_string()),
            knot_types::KnotHostname::new(self.server.hostname.clone())
                .err()
                .map(|_| "server.hostname isn't a valid bare hostname".to_string()),
            check(
                !self.server.admins.is_empty(),
                "server.admins must list at least one DID",
            ),
            check(
                self.repo.scan_path.is_absolute(),
                "repo.scan_path must be absolute path",
            ),
            check(
                self.secrets.sealed_key_file.is_absolute(),
                "secrets.sealed_key_file must be absolute path",
            ),
            check(
                self.server.ssh_host_key_file.is_absolute(),
                "server.ssh_host_key_file must be absolute path",
            ),
            check(
                self.tls.cert_path.is_some() == self.tls.key_path.is_some(),
                "tls.cert_path and tls.key_path must both be set or both unset",
            ),
            self.tls
                .cert_path
                .as_ref()
                .filter(|path| !path.is_absolute())
                .map(|_| "tls.cert_path must be absolute path".to_string()),
            self.tls
                .key_path
                .as_ref()
                .filter(|path| !path.is_absolute())
                .map(|_| "tls.key_path must be absolute path".to_string()),
            check(
                !(self.tls.acme_enabled && self.static_cert_enabled()),
                "tls.acme_enabled cannot combine with a static tls.cert_path and tls.key_path",
            ),
            check(
                !self.tls.acme_enabled || self.tls.acme_cache_dir.is_some(),
                "tls.acme_cache_dir is required when tls.acme_enabled is set",
            ),
            self.tls
                .acme_cache_dir
                .as_ref()
                .filter(|path| !path.is_absolute())
                .map(|_| "tls.acme_cache_dir must be absolute path".to_string()),
            check(
                !self.tls.acme_enabled
                    || self
                        .tls
                        .acme_contact
                        .as_deref()
                        .is_some_and(is_contact_email),
                "tls.acme_contact must be a contact email when tls.acme_enabled is set",
            ),
            check(
                !self.tls.mtls_enabled || self.tls_enabled(),
                "tls.mtls_enabled requires a server certificate via static paths or ACME",
            ),
            check(
                !self.tls.mtls_enabled || self.tls.mtls_client_ca_path.is_some(),
                "tls.mtls_client_ca_path is required when tls.mtls_enabled is set",
            ),
            self.tls
                .mtls_client_ca_path
                .as_ref()
                .filter(|path| !path.is_absolute())
                .map(|_| "tls.mtls_client_ca_path must be absolute path".to_string()),
            check(
                !self.tls.mtls_enabled
                    || self
                        .tls
                        .mtls_admin_spki_pin
                        .as_deref()
                        .is_some_and(is_spki_pin),
                "tls.mtls_admin_spki_pin must be a base64 SHA-256 pin when tls.mtls_enabled is set",
            ),
            check(
                is_env_var_name(&self.secrets.master_key_env),
                "secrets.master_key_env must be valid environment variable name",
            ),
            check(
                self.server.ssh_max_pack_bytes > 0,
                "server.ssh_max_pack_bytes must be greater than zero",
            ),
            check(
                self.server.listen_header_timeout_ms > 0,
                "server.listen_header_timeout_ms must be greater than zero",
            ),
            check(
                self.server.listen_idle_timeout_ms > 0,
                "server.listen_idle_timeout_ms must be greater than zero",
            ),
            check(
                self.server.listen_idle_timeout_ms >= self.server.listen_header_timeout_ms,
                "server.listen_idle_timeout_ms must be at least server.listen_header_timeout_ms",
            ),
            check(
                self.server.listen_max_connections > 0,
                "server.listen_max_connections must be greater than zero",
            ),
            check(
                self.server.listen_rate_limit_per_second > 0,
                "server.listen_rate_limit_per_second must be greater than zero",
            ),
            check(
                self.server.listen_rate_limit_burst > 0,
                "server.listen_rate_limit_burst must be greater than zero",
            ),
            check(
                self.server.listen_max_inflight_requests > 0,
                "server.listen_max_inflight_requests must be greater than zero",
            ),
            check(
                self.server.listen_request_timeout_ms > 0,
                "server.listen_request_timeout_ms must be greater than zero",
            ),
            check(
                self.server.listen_body_timeout_ms > 0,
                "server.listen_body_timeout_ms must be greater than zero",
            ),
            check(
                self.server.listen_write_request_timeout_ms > 0,
                "server.listen_write_request_timeout_ms must be greater than zero",
            ),
            knot_types::RefName::new(format!("refs/heads/{}", self.repo.default_branch))
                .err()
                .map(|_| "repo.default_branch isn't valid branch name".to_string()),
            check(
                self.http.connect_timeout_ms > 0,
                "http.connect_timeout_ms must be greater than zero",
            ),
            check(
                self.http.read_timeout_ms > 0,
                "http.read_timeout_ms must be greater than zero",
            ),
            check(
                self.http.request_timeout_ms > 0,
                "http.request_timeout_ms must be greater than zero",
            ),
            check(
                self.http.max_response_bytes > 0,
                "http.max_response_bytes must be greater than zero",
            ),
            check(
                self.atproto.plc_directory.scheme() == "https",
                "atproto.plc_directory must be https URL",
            ),
            check(
                self.atproto.plc_directory.host().is_some(),
                "atproto.plc_directory must have host",
            ),
            check(
                self.xrpc.max_body_bytes > 0,
                "xrpc.max_body_bytes must be greater than zero",
            ),
            check(
                self.xrpc.max_response_bytes > 0,
                "xrpc.max_response_bytes must be greater than zero",
            ),
            check(
                self.xrpc.max_archive_bytes > 0,
                "xrpc.max_archive_bytes must be greater than zero",
            ),
            check(
                self.xrpc.tree_last_commit_budget_ms > 0,
                "xrpc.tree_last_commit_budget_ms must be greater than zero",
            ),
            check(
                self.xrpc.blob_last_commit_budget_ms > 0,
                "xrpc.blob_last_commit_budget_ms must be greater than zero",
            ),
            check(
                self.xrpc.languages_budget_ms > 0,
                "xrpc.languages_budget_ms must be greater than zero",
            ),
            check(
                self.xrpc.languages_push_budget_ms > 0,
                "xrpc.languages_push_budget_ms must be greater than zero",
            ),
            check(
                self.xrpc.max_patch_bytes > 0,
                "xrpc.max_patch_bytes must be greater than zero",
            ),
            check(
                self.xrpc.max_patch_decompressed_bytes > 0,
                "xrpc.max_patch_decompressed_bytes must be greater than zero",
            ),
            check(
                self.xrpc.fork_max_pack_bytes > 0,
                "xrpc.fork_max_pack_bytes must be greater than zero",
            ),
            check(
                self.xrpc.fork_fetch_timeout_ms > 0,
                "xrpc.fork_fetch_timeout_ms must be greater than zero",
            ),
            check(
                !self.git.user_name.trim().is_empty(),
                "git.user_name mustn't be empty",
            ),
            check(
                !self.git.user_email.trim().is_empty(),
                "git.user_email mustn't be empty",
            ),
            check(
                self.object_format().is_some(),
                "git.object_format must be \"sha1\" or \"sha256\"",
            ),
            check(
                self.xrpc.preauth_burst > 0,
                "xrpc.preauth_burst must be greater than zero",
            ),
            check(
                self.xrpc.preauth_refill_ms > 0,
                "xrpc.preauth_refill_ms must be greater than zero",
            ),
            check(
                self.xrpc.per_peer_inflight > 0,
                "xrpc.per_peer_inflight must be greater than zero",
            ),
            check(
                self.xrpc.global_inflight >= self.xrpc.per_peer_inflight,
                "xrpc.global_inflight must be at least xrpc.per_peer_inflight",
            ),
            check(
                self.xrpc.per_actor_reservations > 0,
                "xrpc.per_actor_reservations must be greater than zero",
            ),
            check(
                self.xrpc.max_pending_reservations >= self.xrpc.per_actor_reservations,
                "xrpc.max_pending_reservations must be at least xrpc.per_actor_reservations",
            ),
            check(
                self.xrpc.reservation_ttl_secs > 0,
                "xrpc.reservation_ttl_secs must be greater than zero",
            ),
            check(
                self.xrpc.events_replay_buffer > 0,
                "xrpc.events_replay_buffer must be greater than zero",
            ),
            check(
                self.xrpc.events_replay_bytes > 0,
                "xrpc.events_replay_bytes must be greater than zero",
            ),
            check(
                self.xrpc.events_max_subscribers > 0,
                "xrpc.events_max_subscribers must be greater than zero",
            ),
            check(
                self.xrpc.events_max_per_peer > 0,
                "xrpc.events_max_per_peer must be greater than zero",
            ),
            check(
                self.xrpc.events_max_subscribers >= self.xrpc.events_max_per_peer,
                "xrpc.events_max_subscribers must be at least xrpc.events_max_per_peer",
            ),
            check(
                self.maintenance.interval_secs > 0,
                "maintenance.interval_secs must be greater than zero",
            ),
            check(
                self.maintenance.repack_max_objects > 0,
                "maintenance.repack_max_objects must be greater than zero",
            ),
            check(
                self.maintenance.repack_geometric_factor >= 2,
                "maintenance.repack_geometric_factor must be at least 2",
            ),
            check(
                self.maintenance.large_push_bytes > 0,
                "maintenance.large_push_bytes must be greater than zero",
            ),
            check(
                self.pack_cache.ttl_secs > 0,
                "pack_cache.ttl_secs must be greater than zero",
            ),
            check(
                self.pack_cache.max_entry_bytes > 0,
                "pack_cache.max_entry_bytes must be greater than zero",
            ),
            check(
                self.pack_cache.max_total_bytes > 0,
                "pack_cache.max_total_bytes must be greater than zero",
            ),
            check(
                self.pack_cache.max_total_bytes >= self.pack_cache.max_entry_bytes,
                "pack_cache.max_total_bytes must be at least pack_cache.max_entry_bytes",
            ),
            check(
                self.pack.max_objects > 0,
                "pack.max_objects must be greater than zero",
            ),
            check(
                self.pack.max_total_bytes > 0,
                "pack.max_total_bytes must be greater than zero",
            ),
            check(
                self.pack.selection_max_objects > 0,
                "pack.selection_max_objects must be greater than zero",
            ),
            check(
                self.pack.selection_time_budget_secs > 0,
                "pack.selection_time_budget_secs must be greater than zero",
            ),
            self.lfs
                .store_path
                .as_ref()
                .filter(|path| !path.is_absolute())
                .map(|_| "lfs.store_path must be absolute path".to_string()),
            self.lfs
                .store_path
                .as_ref()
                .filter(|path| {
                    path.starts_with(&self.repo.scan_path) || self.repo.scan_path.starts_with(path)
                })
                .map(|_| "lfs.store_path mustn't overlap repo.scan_path".to_string()),
            check(
                self.lfs.max_object_bytes > 0,
                "lfs.max_object_bytes must be greater than zero",
            ),
            check(
                self.lfs.gc_interval_secs > 0,
                "lfs.gc_interval_secs must be greater than zero",
            ),
            check(
                self.lfs.max_ssh_transfers > 0,
                "lfs.max_ssh_transfers must be greater than zero",
            ),
            check(
                self.lfs.max_http_downloads > 0,
                "lfs.max_http_downloads must be greater than zero",
            ),
            self.xrpc
                .trusted_proxy_header
                .as_ref()
                .filter(|header| !is_http_token(header))
                .map(|_| "xrpc.trusted_proxy_header isn't valid HTTP header name".to_string()),
            match self.homepage.source() {
                HomepageSource::File(path) if !path.is_absolute() => {
                    Some("homepage.path must be absolute path".to_string())
                }
                _ => None,
            },
        ]
        .into_iter()
        .flatten()
        .chain(self.port_collisions())
        .collect();

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigError { errors })
        }
    }

    fn port_collisions(&self) -> Vec<String> {
        let binds = [
            ("server.listen_addr", self.server.listen_addr),
            (
                "server.internal_listen_addr",
                self.server.internal_listen_addr,
            ),
            ("server.ssh_listen_addr", self.server.ssh_listen_addr),
        ];
        [(0, 1), (0, 2), (1, 2)]
            .into_iter()
            .filter(|&(a, b)| binds[a].1.port() == binds[b].1.port())
            .map(|(a, b)| {
                format!(
                    "{} and {} cannot bind same port {}",
                    binds[a].0,
                    binds[b].0,
                    binds[a].1.port()
                )
            })
            .collect()
    }
}

fn check(ok: bool, message: &str) -> Option<String> {
    (!ok).then(|| message.to_string())
}

fn is_http_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

fn is_contact_email(value: &str) -> bool {
    let mut parts = value.splitn(2, '@');
    matches!(
        (parts.next(), parts.next()),
        (Some(local), Some(domain))
            if !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !value.chars().any(char::is_whitespace)
    )
}

fn is_spki_pin(value: &str) -> bool {
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .is_ok_and(|bytes| bytes.len() == 32)
}

fn is_env_var_name(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with(|c: char| c.is_ascii_digit())
        && value
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

#[derive(Debug, thiserror::Error)]
pub struct ConfigError {
    pub errors: Vec<String>,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{} configuration problem(s):", self.errors.len())?;
        self.errors
            .iter()
            .try_for_each(|error| writeln!(f, " - {error}"))
    }
}

pub struct Validated(KnotConfig);

impl Validated {
    pub fn verify_environment(&self) -> Result<(), EnvError> {
        verify_writable_dir("repo.scan_path", &self.0.repo.scan_path)?;
        self.0
            .lfs
            .store_path
            .as_deref()
            .map_or(Ok(()), |path| verify_writable_dir("lfs.store_path", path))?;
        verify_homepage(self.0.homepage.source())?;
        verify_master_key(&self.0.secrets.master_key_env)
    }

    pub fn into_inner(self) -> KnotConfig {
        self.0
    }
}

impl std::ops::Deref for Validated {
    type Target = KnotConfig;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EnvError {
    #[error("{field} {path} isn't accessible")]
    DirInaccessible {
        field: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{field} {path} isn't directory")]
    DirNotDir { field: &'static str, path: PathBuf },
    #[error("{field} {path} isn't writable")]
    DirNotWritable {
        field: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("homepage.path {path} isn't accessible")]
    HomepageInaccessible {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("homepage.path {path} isn't a regular file")]
    HomepageNotFile { path: PathBuf },
    #[error("master key env var {name} isn't set")]
    MasterKeyUnset { name: String },
    #[error("master key env var {name} is empty")]
    MasterKeyEmpty { name: String },
    #[error("master key env var {name} isn't valid base64")]
    MasterKeyNotBase64 {
        name: String,
        #[source]
        source: base64::DecodeError,
    },
    #[error(
        "master key env var {name} decodes to only {len} of minimum {MASTER_KEY_MIN_BYTES} bytes"
    )]
    MasterKeyTooShort { name: String, len: usize },
}

const MASTER_KEY_MIN_BYTES: usize = 32;

fn verify_writable_dir(field: &'static str, path: &Path) -> Result<(), EnvError> {
    let metadata = std::fs::metadata(path).map_err(|source| EnvError::DirInaccessible {
        field,
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(EnvError::DirNotDir {
            field,
            path: path.to_path_buf(),
        });
    }
    tempfile::Builder::new()
        .prefix(".knot-write-probe")
        .tempfile_in(path)
        .map(drop)
        .map_err(|source| EnvError::DirNotWritable {
            field,
            path: path.to_path_buf(),
            source,
        })
}

fn verify_homepage(source: HomepageSource) -> Result<(), EnvError> {
    let HomepageSource::File(path) = source else {
        return Ok(());
    };
    let file = std::fs::File::open(&path).map_err(|source| EnvError::HomepageInaccessible {
        path: path.clone(),
        source,
    })?;
    let is_file = file
        .metadata()
        .map_err(|source| EnvError::HomepageInaccessible {
            path: path.clone(),
            source,
        })?
        .is_file();
    is_file
        .then_some(())
        .ok_or(EnvError::HomepageNotFile { path })
}

fn verify_master_key(name: &str) -> Result<(), EnvError> {
    let value = std::env::var(name).ok();
    validate_master_key(name, value.as_deref())
}

fn validate_master_key(name: &str, value: Option<&str>) -> Result<(), EnvError> {
    let raw = value.ok_or_else(|| EnvError::MasterKeyUnset {
        name: name.to_string(),
    })?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(EnvError::MasterKeyEmpty {
            name: name.to_string(),
        });
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .map_err(|source| EnvError::MasterKeyNotBase64 {
            name: name.to_string(),
            source,
        })?;
    (decoded.len() >= MASTER_KEY_MIN_BYTES)
        .then_some(())
        .ok_or(EnvError::MasterKeyTooShort {
            name: name.to_string(),
            len: decoded.len(),
        })
}

static CONFIG: OnceLock<KnotConfig> = OnceLock::new();

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("config file not found: {0}")]
    Missing(PathBuf),
    #[error(transparent)]
    Confique(#[from] confique::Error),
    #[error(transparent)]
    Invalid(#[from] ConfigError),
}

pub fn load(path: Option<&Path>) -> Result<Validated, LoadError> {
    if let Some(path) = path
        && !path.exists()
    {
        return Err(LoadError::Missing(path.to_path_buf()));
    }
    let mut builder = KnotConfig::builder().env();
    if let Some(path) = path {
        builder = builder.file(path);
    }
    let config = builder.file("/etc/knot/config.toml").load()?;
    config.validate()?;
    Ok(Validated(config))
}

pub fn template() -> String {
    confique::toml::template::<KnotConfig>(confique::toml::FormatOptions::default())
}

pub fn init(config: Validated) {
    CONFIG
        .set(config.into_inner())
        .expect("knot-config: configuration already initialized");
}

pub fn get() -> &'static KnotConfig {
    CONFIG
        .get()
        .expect("knot-config: not initialized, call knot_config::init first")
}

pub fn try_get() -> Option<&'static KnotConfig> {
    CONFIG.get()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_toml_is_the_generated_template() {
        assert_eq!(
            template(),
            include_str!("../../../example.toml"),
            "regenerate example.toml from knot_config::template() after changing config"
        );
    }

    fn sample() -> KnotConfig {
        KnotConfig {
            server: ServerConfig {
                hostname: "oyster.cafe".to_string(),
                admins: vec![AccountDid::new("did:plc:nel").unwrap()],
                listen_addr: "[::]:5555".parse().unwrap(),
                listen_header_timeout_ms: 10_000,
                listen_idle_timeout_ms: 60_000,
                listen_max_connections: 1_024,
                listen_rate_limit_per_second: 50,
                listen_rate_limit_burst: 200,
                listen_max_inflight_requests: 1_024,
                listen_request_timeout_ms: 60_000,
                listen_body_timeout_ms: 30_000,
                listen_write_request_timeout_ms: 1_800_000,
                internal_listen_addr: "[::1]:5444".parse().unwrap(),
                ssh_listen_addr: "[::]:2222".parse().unwrap(),
                ssh_host_key_file: PathBuf::from("/var/lib/knot/ssh_host_key"),
                ssh_max_pack_bytes: 8_589_934_592,
                appview_endpoint: AppviewEndpoint::new("https://tangled.org").unwrap(),
            },
            tls: TlsConfig {
                cert_path: None,
                key_path: None,
                http3: true,
                acme_enabled: false,
                acme_cache_dir: None,
                acme_contact: None,
                acme_staging: false,
                mtls_enabled: false,
                mtls_client_ca_path: None,
                mtls_admin_spki_pin: None,
            },
            acl: AclConfig {
                admission: AdmissionPolicy::Closed,
            },
            repo: RepoConfig {
                scan_path: PathBuf::from("/srv/git"),
                default_branch: "main".to_string(),
            },
            git: GitConfig {
                user_name: "Tangled".to_string(),
                user_email: "noreply@tangled.sh".to_string(),
                object_format: "sha1".to_string(),
            },
            secrets: SecretsConfig {
                sealed_key_file: PathBuf::from("/var/lib/knot/keys.sealed"),
                master_key_env: "KNOT_MASTER_KEY".to_string(),
            },
            http: HttpConfig {
                connect_timeout_ms: 5_000,
                read_timeout_ms: 30_000,
                request_timeout_ms: 60_000,
                max_response_bytes: 16_777_216,
            },
            atproto: AtprotoConfig {
                plc_directory: Url::parse("https://plc.nel.pet/").unwrap(),
            },
            xrpc: XrpcConfig {
                max_body_bytes: 65_536,
                max_response_bytes: 5_242_880,
                max_archive_bytes: 1_073_741_824,
                tree_last_commit_budget_ms: 300,
                blob_last_commit_budget_ms: 2_000,
                languages_budget_ms: 1_000,
                languages_push_budget_ms: 2_000,
                max_patch_bytes: 16_777_216,
                max_patch_decompressed_bytes: 134_217_728,
                preauth_burst: 20,
                preauth_refill_ms: 100,
                per_peer_inflight: 8,
                global_inflight: 64,
                max_pending_reservations: 256,
                per_actor_reservations: 32,
                reservation_ttl_secs: 3_600,
                fork_max_pack_bytes: 1_073_741_824,
                fork_fetch_timeout_ms: 600_000,
                trusted_proxy_header: None,
                events_replay_buffer: 4_096,
                events_replay_bytes: 67_108_864,
                events_max_subscribers: 256,
                events_max_per_peer: 8,
            },
            maintenance: MaintenanceConfig {
                enabled: true,
                commit_graph: true,
                multi_pack_index: true,
                bitmap: true,
                interval_secs: 21_600,
                repack_max_objects: 16_000_000,
                repack_geometric_factor: 2,
                prune_grace_secs: 1_209_600,
                reflog_expire_secs: 7_776_000,
                large_push_bytes: 52_428_800,
            },
            pack_cache: PackCacheConfig {
                enabled: true,
                ttl_secs: 60,
                max_entry_bytes: 67_108_864,
                max_total_bytes: 536_870_912,
            },
            pack: PackConfig {
                max_objects: 16_000_000,
                max_total_bytes: 68_719_476_736,
                selection_max_objects: 16_000_000,
                selection_time_budget_secs: 600,
            },
            lfs: LfsConfig {
                store_path: None,
                max_object_bytes: 5_368_709_120,
                free_space_floor_bytes: 1_073_741_824,
                gc_grace_secs: 1_209_600,
                gc_interval_secs: 21_600,
                max_ssh_transfers: 16,
                max_http_downloads: 64,
            },
            resources: ResourcesConfig {
                max_threads: 0,
                max_memory_bytes: 0,
            },
            messages: knot_messages::MessagesConfig::defaults(),
            homepage: HomepageConfig {
                enabled: true,
                path: None,
            },
            ci: CiConfig {
                logs_addr: Some("logs.oyster.cafe:3333".to_string()),
            },
        }
    }

    fn apply_acme(config: &mut KnotConfig) {
        config.tls.acme_enabled = true;
        config.tls.acme_cache_dir = Some(PathBuf::from("/var/lib/knot/acme"));
        config.tls.acme_contact = Some("nel@oyster.cafe".to_string());
    }

    fn apply_mtls(config: &mut KnotConfig) {
        config.tls.cert_path = Some(PathBuf::from("/etc/knot/tls/cert.pem"));
        config.tls.key_path = Some(PathBuf::from("/etc/knot/tls/key.pem"));
        config.tls.mtls_enabled = true;
        config.tls.mtls_client_ca_path = Some(PathBuf::from("/etc/knot/tls/admin-ca.pem"));
        config.tls.mtls_admin_spki_pin =
            Some(base64::engine::general_purpose::STANDARD.encode([7u8; 32]));
    }

    #[test]
    fn accepts_valid_config() {
        type Case = (&'static str, fn(&mut KnotConfig), bool, bool);
        let cases: &[Case] = &[
            ("valid_config_passes", |_| {}, false, false),
            (
                "matched_absolute_tls_paths",
                |config| {
                    config.tls.cert_path = Some(PathBuf::from("/etc/knot/tls/cert.pem"));
                    config.tls.key_path = Some(PathBuf::from("/etc/knot/tls/key.pem"));
                },
                true,
                true,
            ),
            ("acme_enables_tls", apply_acme, true, false),
            ("mtls_with_server_cert_and_pin", apply_mtls, true, true),
            (
                "an_immediate_prune_grace",
                |config| config.maintenance.prune_grace_secs = 0,
                false,
                false,
            ),
        ];
        cases
            .iter()
            .for_each(|(label, mutate, tls_enabled, static_cert)| {
                let mut config = sample();
                mutate(&mut config);
                assert!(config.validate().is_ok(), "{label}");
                assert_eq!(config.tls_enabled(), *tls_enabled, "{label} tls_enabled");
                assert_eq!(
                    config.static_cert_enabled(),
                    *static_cert,
                    "{label} static_cert_enabled"
                );
            });
    }

    #[test]
    fn rejects_invalid_config() {
        type Case = (&'static str, fn(&mut KnotConfig), &'static str);
        let cases: &[Case] = &[
            (
                "a_cert_without_a_key",
                |config| config.tls.cert_path = Some(PathBuf::from("/etc/knot/tls/cert.pem")),
                "both be set or both unset",
            ),
            (
                "a_relative_cert_path",
                |config| {
                    config.tls.cert_path = Some(PathBuf::from("tls/cert.pem"));
                    config.tls.key_path = Some(PathBuf::from("tls/key.pem"));
                },
                "tls.cert_path",
            ),
            (
                "acme_cannot_combine_with_static",
                |config| {
                    apply_acme(config);
                    config.tls.cert_path = Some(PathBuf::from("/etc/knot/tls/cert.pem"));
                    config.tls.key_path = Some(PathBuf::from("/etc/knot/tls/key.pem"));
                },
                "cannot combine",
            ),
            (
                "acme_without_a_cache_dir",
                |config| {
                    apply_acme(config);
                    config.tls.acme_cache_dir = None;
                },
                "tls.acme_cache_dir is required",
            ),
            (
                "acme_without_a_valid_contact",
                |config| {
                    apply_acme(config);
                    config.tls.acme_contact = Some("not-an-email".to_string());
                },
                "tls.acme_contact",
            ),
            (
                "mtls_without_a_server_certificate",
                |config| {
                    apply_mtls(config);
                    config.tls.cert_path = None;
                    config.tls.key_path = None;
                },
                "tls.mtls_enabled requires a server certificate",
            ),
            (
                "mtls_with_a_malformed_pin",
                |config| {
                    apply_mtls(config);
                    config.tls.mtls_admin_spki_pin =
                        Some(base64::engine::general_purpose::STANDARD.encode([0u8; 16]));
                },
                "tls.mtls_admin_spki_pin",
            ),
            (
                "empty_admin_list",
                |config| config.server.admins = Vec::new(),
                "admins",
            ),
            (
                "a_zero_maintenance_interval",
                |config| config.maintenance.interval_secs = 0,
                "maintenance.interval_secs",
            ),
            (
                "a_zero_repack_object_limit",
                |config| config.maintenance.repack_max_objects = 0,
                "maintenance.repack_max_objects",
            ),
            (
                "a_geometric_factor_below_two",
                |config| config.maintenance.repack_geometric_factor = 1,
                "maintenance.repack_geometric_factor",
            ),
            (
                "a_zero_large_push_threshold",
                |config| config.maintenance.large_push_bytes = 0,
                "maintenance.large_push_bytes",
            ),
            (
                "a_zero_pack_cache_ttl",
                |config| config.pack_cache.ttl_secs = 0,
                "pack_cache.ttl_secs",
            ),
            (
                "a_zero_pack_cache_entry_limit",
                |config| config.pack_cache.max_entry_bytes = 0,
                "pack_cache.max_entry_bytes",
            ),
            (
                "a_zero_pack_cache_total_limit",
                |config| config.pack_cache.max_total_bytes = 0,
                "pack_cache.max_total_bytes",
            ),
            (
                "a_pack_cache_total_below_one_entry",
                |config| {
                    config.pack_cache.max_entry_bytes = 1_000;
                    config.pack_cache.max_total_bytes = 500;
                },
                "at least pack_cache.max_entry_bytes",
            ),
            (
                "relative_scan_path",
                |config| config.repo.scan_path = PathBuf::from("relative/git"),
                "scan_path",
            ),
            (
                "a_relative_lfs_store_path",
                |config| config.lfs.store_path = Some(PathBuf::from("relative/lfs")),
                "lfs.store_path must be absolute path",
            ),
            (
                "an_lfs_store_inside_the_scan_path",
                |config| config.lfs.store_path = Some(config.repo.scan_path.join("lfs")),
                "lfs.store_path mustn't overlap repo.scan_path",
            ),
            (
                "a_scan_path_inside_the_lfs_store",
                |config| {
                    config.lfs.store_path = Some(PathBuf::from("/srv/media"));
                    config.repo.scan_path = PathBuf::from("/srv/media/git");
                },
                "lfs.store_path mustn't overlap repo.scan_path",
            ),
            (
                "a_zero_lfs_object_limit",
                |config| config.lfs.max_object_bytes = 0,
                "lfs.max_object_bytes",
            ),
            (
                "a_zero_lfs_gc_interval",
                |config| config.lfs.gc_interval_secs = 0,
                "lfs.gc_interval_secs",
            ),
            (
                "a_zero_lfs_ssh_transfer_limit",
                |config| config.lfs.max_ssh_transfers = 0,
                "lfs.max_ssh_transfers",
            ),
            (
                "a_zero_lfs_http_download_limit",
                |config| config.lfs.max_http_downloads = 0,
                "lfs.max_http_downloads",
            ),
            (
                "bad_master_key_env_name",
                |config| config.secrets.master_key_env = "9 bad name".to_string(),
                "master_key_env",
            ),
            (
                "zero_http_timeout",
                |config| config.http.request_timeout_ms = 0,
                "request_timeout_ms",
            ),
            (
                "zero_tree_last_commit_budget",
                |config| config.xrpc.tree_last_commit_budget_ms = 0,
                "tree_last_commit_budget_ms",
            ),
            (
                "zero_blob_last_commit_budget",
                |config| config.xrpc.blob_last_commit_budget_ms = 0,
                "blob_last_commit_budget_ms",
            ),
            (
                "zero_languages_budget",
                |config| config.xrpc.languages_budget_ms = 0,
                "languages_budget_ms",
            ),
            (
                "zero_languages_push_budget",
                |config| config.xrpc.languages_push_budget_ms = 0,
                "languages_push_budget_ms",
            ),
            (
                "zero_events_replay_buffer",
                |config| config.xrpc.events_replay_buffer = 0,
                "events_replay_buffer",
            ),
            (
                "zero_events_replay_bytes",
                |config| config.xrpc.events_replay_bytes = 0,
                "events_replay_bytes",
            ),
            (
                "zero_events_max_subscribers",
                |config| config.xrpc.events_max_subscribers = 0,
                "events_max_subscribers",
            ),
            (
                "zero_events_max_per_peer",
                |config| config.xrpc.events_max_per_peer = 0,
                "events_max_per_peer",
            ),
            (
                "a_per_peer_limit_above_the_global_limit",
                |config| {
                    config.xrpc.events_max_subscribers = 4;
                    config.xrpc.events_max_per_peer = 8;
                },
                "events_max_subscribers must be at least",
            ),
            (
                "a_non_https_plc_directory",
                |config| {
                    config.atproto.plc_directory = Url::parse("http://plc.nel.pet/").unwrap();
                },
                "plc_directory",
            ),
            (
                "an_idle_timeout_below_the_header_timeout",
                |config| {
                    config.server.listen_header_timeout_ms = 10_000;
                    config.server.listen_idle_timeout_ms = 5_000;
                },
                "listen_idle_timeout_ms must be at least",
            ),
            (
                "colliding_bind_ports",
                |config| config.server.internal_listen_addr = config.server.listen_addr,
                "same port",
            ),
            (
                "a_relative_homepage_path",
                |config| config.homepage.path = Some(PathBuf::from("homepage.html")),
                "homepage.path must be absolute path",
            ),
        ];
        cases.iter().for_each(|(label, mutate, expected)| {
            let mut config = sample();
            mutate(&mut config);
            let errors = config.validate().unwrap_err().errors;
            assert!(
                errors.iter().any(|error| error.contains(expected)),
                "{label}: expected an error containing {expected}, got {errors:?}"
            );
        });
    }

    #[test]
    fn homepage_source_resolves_states() {
        let disabled = HomepageConfig {
            enabled: false,
            path: Some(PathBuf::from("/etc/knot/home.html")),
        };
        assert!(matches!(disabled.source(), HomepageSource::Disabled));

        let default = HomepageConfig {
            enabled: true,
            path: None,
        };
        assert!(matches!(default.source(), HomepageSource::Default));

        let file = HomepageConfig {
            enabled: true,
            path: Some(PathBuf::from("/etc/knot/home.html")),
        };
        match file.source() {
            HomepageSource::File(path) => assert_eq!(path, PathBuf::from("/etc/knot/home.html")),
            other => panic!("expected File, got {other:?}"),
        }
    }

    #[test]
    fn validates_master_key() {
        let short = base64::engine::general_purpose::STANDARD.encode([0u8; 16]);
        let key = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        type Case<'a> = (Option<&'a str>, fn(&Result<(), EnvError>) -> bool);
        let cases: Vec<Case<'_>> = vec![
            (None, |result| {
                matches!(result, Err(EnvError::MasterKeyUnset { .. }))
            }),
            (Some("   "), |result| {
                matches!(result, Err(EnvError::MasterKeyEmpty { .. }))
            }),
            (Some("not base64 *** value"), |result| {
                matches!(result, Err(EnvError::MasterKeyNotBase64 { .. }))
            }),
            (Some(short.as_str()), |result| {
                matches!(result, Err(EnvError::MasterKeyTooShort { .. }))
            }),
            (Some(key.as_str()), |result| result.is_ok()),
        ];
        cases.iter().for_each(|(input, expect)| {
            assert!(expect(&validate_master_key("KNOT_MASTER_KEY", *input)));
        });
    }

    #[test]
    fn admins_parse_from_comma_separated_env() {
        let parsed = parse_admins("did:plc:nel, did:plc:olaren").unwrap();
        assert_eq!(parsed.len(), 2);
        assert!(parse_admins("not-a-did").is_err());
    }

    #[test]
    fn http_limits_map_from_config() {
        let limits = sample().http_limits();
        assert_eq!(limits.connect_timeout, Duration::from_millis(5_000));
        assert_eq!(limits.request_timeout, Duration::from_millis(60_000));
        assert_eq!(limits.max_response_bytes, 16_777_216);
    }

    #[test]
    fn missing_dir_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("no-such-dir");
        assert!(matches!(
            verify_writable_dir("repo.scan_path", &absent),
            Err(EnvError::DirInaccessible { .. })
        ));
    }

    #[test]
    fn file_in_place_of_dir_is_rejected() {
        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(matches!(
            verify_writable_dir("lfs.store_path", file.path()),
            Err(EnvError::DirNotDir { .. })
        ));
    }

    #[test]
    fn writable_dir_passes() {
        let dir = tempfile::tempdir().unwrap();
        assert!(verify_writable_dir("repo.scan_path", dir.path()).is_ok());
    }

    #[test]
    fn verify_homepage_accepts_absent_and_default_sources() {
        assert!(verify_homepage(HomepageSource::Disabled).is_ok());
        assert!(verify_homepage(HomepageSource::Default).is_ok());
    }

    #[test]
    fn verify_homepage_rejects_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("no-such-page.html");
        assert!(matches!(
            verify_homepage(HomepageSource::File(absent)),
            Err(EnvError::HomepageInaccessible { .. })
        ));
    }

    #[test]
    fn verify_homepage_rejects_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            verify_homepage(HomepageSource::File(dir.path().to_path_buf())),
            Err(EnvError::HomepageNotFile { .. })
        ));
    }

    #[test]
    fn verify_homepage_accepts_readable_file() {
        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(verify_homepage(HomepageSource::File(file.path().to_path_buf())).is_ok());
    }

    #[test]
    fn disabled_homepage_ignores_relative_path() {
        let mut config = sample();
        config.homepage.enabled = false;
        config.homepage.path = Some(PathBuf::from("homepage.html"));
        assert!(config.validate().is_ok());
    }
}
