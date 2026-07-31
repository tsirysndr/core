use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, anyhow};
use confique::Config;
use trusted_proxies::{ProxyNetError, TrustedProxies};
use url::Url;

const SYSTEM_CONFIG_PATH: &str = "/etc/bobbin/config.toml";
const ENV_PREFIX: &str = "BOBBIN_";

const KNOWN_KEYS: &[&str] = &[
    "server.binds",
    "server.shutdown_grace_secs",
    "server.debug_bind",
    "server.trusted_proxies",
    "hydrant.url",
    "hydrant.start_cursor",
    "ingest.parallelism",
    "backpressure.per_request_anon_bytes",
    "backpressure.adjust_interval_ms",
    "backpressure.relieve_below_ratio",
    "backpressure.tighten_above_ratio",
    "backpressure.reserved_index_bytes",
    "slingshot.url",
    "record_cache.lru_bytes",
    "search.heap_bytes",
    "knot.allow_private",
    "knot.require_https",
    "mirror.url",
    "log.format",
    "log.filter",
];

const KNOWN_ENVS: &[&str] = &[
    "BOBBIN_CONFIG",
    "BOBBIN_BIND",
    "BOBBIN_SHUTDOWN_GRACE_SECS",
    "BOBBIN_DEBUG_BIND",
    "BOBBIN_TRUSTED_PROXIES",
    "BOBBIN_HYDRANT_URL",
    "BOBBIN_START_CURSOR",
    "BOBBIN_INGEST_PARALLELISM",
    "BOBBIN_BACKPRESSURE_PER_REQUEST_ANON_BYTES",
    "BOBBIN_BACKPRESSURE_ADJUST_INTERVAL_MS",
    "BOBBIN_BACKPRESSURE_RELIEVE_BELOW_RATIO",
    "BOBBIN_BACKPRESSURE_TIGHTEN_ABOVE_RATIO",
    "BOBBIN_BACKPRESSURE_RESERVED_INDEX_BYTES",
    "BOBBIN_SLINGSHOT_URL",
    "BOBBIN_RECORD_LRU_BYTES",
    "BOBBIN_SEARCH_HEAP_BYTES",
    "BOBBIN_KNOT_ALLOW_PRIVATE",
    "BOBBIN_KNOT_REQUIRE_HTTPS",
    "BOBBIN_MIRROR_URL",
    "BOBBIN_LOG_FORMAT",
    "BOBBIN_LOG",
];

#[derive(Debug, Config)]
pub struct BobbinConfig {
    #[config(nested)]
    pub server: ServerConfig,

    #[config(nested)]
    pub hydrant: HydrantConfig,

    #[config(nested)]
    pub ingest: IngestConfig,

    #[config(nested)]
    pub backpressure: BackpressureConfig,

    #[config(nested)]
    pub slingshot: SlingshotConfig,

    #[config(nested)]
    pub record_cache: RecordCacheConfig,

    #[config(nested)]
    pub search: SearchConfig,

    #[config(nested)]
    pub knot: KnotConfig,

    #[config(nested)]
    pub mirror: MirrorConfig,

    #[config(nested)]
    pub log: LogConfig,
}

#[derive(Debug, Config)]
pub struct ServerConfig {
    /// Addresses the XRPC server listens on. When using as an env var, comma-separated.
    #[config(
        env = "BOBBIN_BIND",
        parse_env = parse_binds,
        default = ["127.0.0.1:8090", "[::1]:8090"]
    )]
    pub binds: Vec<SocketAddr>,

    /// The amount of time in seconds to allow in-flight requests to drain after sigterm
    /// before forcing the listener closed.
    #[config(env = "BOBBIN_SHUTDOWN_GRACE_SECS", default = 30)]
    pub shutdown_grace_secs: u64,

    /// Address for the `/debug/mem` and `/debug/heap` introspection endpoints,
    /// for example `127.0.0.1:8091`. Empty disables them so the debug surface is
    /// never reachable on the public listener. Bind to loopback only.
    #[config(env = "BOBBIN_DEBUG_BIND", default = "")]
    pub debug_bind: String,

    /// Reverse proxies in front of bobbin,
    /// each a bare IP address without a port or a CIDR block such as `173.245.48.0/20`.
    /// Bobbin will read the client address out of `x-forwarded-for`
    /// and forward that one address to the knot
    /// when a request arrives from a proxy on this list,
    /// so a knot that lists bobbin under its own `xrpc.trusted_proxies`
    /// can rate-limit per browser
    /// instead of pooling everyone bobbin serves into a single bucket.
    /// Bobbin will read the last 32 entries of the chain, at most.
    /// Leave empty when bobbin takes connections directly,
    /// since bobbin would otherwise believe a header any client can write.
    /// When using as an env var, comma-separated.
    #[config(
        env = "BOBBIN_TRUSTED_PROXIES",
        parse_env = trusted_proxies::comma_separated,
        default = []
    )]
    pub trusted_proxies: Vec<String>,
}

impl ServerConfig {
    pub fn trusted_proxies(&self) -> Result<TrustedProxies, ProxyNetError> {
        TrustedProxies::parse(self.trusted_proxies.iter().map(String::as_str))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BindParseError {
    #[error("BOBBIN_BIND must list at least one address")]
    Empty,
    #[error("invalid bind entry `{0}`: {1}")]
    Invalid(String, std::net::AddrParseError),
}

fn parse_binds(raw: &str) -> Result<Vec<SocketAddr>, BindParseError> {
    let addrs: Vec<SocketAddr> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| SocketAddr::from_str(s).map_err(|e| BindParseError::Invalid(s.to_owned(), e)))
        .collect::<Result<_, _>>()?;
    if addrs.is_empty() {
        return Err(BindParseError::Empty);
    }
    Ok(addrs)
}

#[derive(Debug, Config)]
pub struct HydrantConfig {
    /// Base URL of the hydrant instance - the cursor-replayable /stream lives
    /// under this. Use `ws://` or `wss://` - `http://` and `https://`
    /// are rewritten to the corresponding ws scheme at connection time!
    #[config(env = "BOBBIN_HYDRANT_URL", default = "http://127.0.0.1:13010")]
    pub url: Url,

    /// The cursor to request on first connect. Reconnects ignore this and resume
    /// strictly after the last cursor seen internally.
    #[config(env = "BOBBIN_START_CURSOR", default = 0)]
    pub start_cursor: u64,
}

#[derive(Debug, Config)]
pub struct IngestConfig {
    /// Concurrent in-flight resolves during ingest. One slot per slingshot rtt.
    /// Default 16 sits at the measured throughput
    /// knee for cold replay against a healthy hydrant. The committer stays
    /// serial so that like, cursor and Spur allocation order are preserved.
    #[config(env = "BOBBIN_INGEST_PARALLELISM", default = 16)]
    pub parallelism: usize,
}

#[derive(Debug, Config)]
pub struct BackpressureConfig {
    /// Estimated peak transient anonymous bytes a single hydrating request holds:
    /// roughly the page of decoded records plus its serialized copy. Combined with
    /// the detected cgroup memory limit to size the in-flight cap on heavy
    /// endpoints. Has no effect when no cgroup memory limit is present, the heavy
    /// path stays unbounded exactly as before.
    #[config(
        env = "BOBBIN_BACKPRESSURE_PER_REQUEST_ANON_BYTES",
        default = 2_097_152
    )]
    pub per_request_anon_bytes: u64,

    /// How often the adaptive watcher samples memory.current/memory.max and
    /// adjusts the in-flight cap. Ignored when no cgroup memory limit is present.
    #[config(env = "BOBBIN_BACKPRESSURE_ADJUST_INTERVAL_MS", default = 500)]
    pub adjust_interval_ms: u64,

    /// memory.current/memory.max ratio below which the watcher additively raises
    /// the in-flight cap back toward its ceiling. Sits below the memory.high
    /// throttle band so the kernel, not this loop, handles mild pressure.
    #[config(env = "BOBBIN_BACKPRESSURE_RELIEVE_BELOW_RATIO", default = 0.85)]
    pub relieve_below_ratio: f64,

    /// memory.current/memory.max ratio above which the watcher multiplicatively
    /// cuts the in-flight cap and purges jemalloc arenas. Set above the memory.high
    /// watermark so this loop acts as the anti-OOM backstop, not a duplicate throttle.
    #[config(env = "BOBBIN_BACKPRESSURE_TIGHTEN_ABOVE_RATIO", default = 0.92)]
    pub tighten_above_ratio: f64,

    /// Bytes held back for the edge index, state indexes, and other derived
    /// state that grows after startup. Folded into the heavy-request reserve so
    /// the static in-flight cap leaves the index room to fill. Has no effect when
    /// no cgroup memory limit is present.
    #[config(env = "BOBBIN_BACKPRESSURE_RESERVED_INDEX_BYTES", default = 67_108_864)]
    pub reserved_index_bytes: u64,
}

#[derive(Debug, Config)]
pub struct SlingshotConfig {
    /// Base URL of a slingshot instance. Used for record bodies and identity.
    #[config(env = "BOBBIN_SLINGSHOT_URL", default = "http://127.0.0.1:13011")]
    pub url: Url,
}

#[derive(Debug, Config)]
pub struct RecordCacheConfig {
    /// Bound of bytes on the in-process record LRU. Records evict on a weighted
    /// LRU policy keyed on URI plus payload length.
    #[config(env = "BOBBIN_RECORD_LRU_BYTES", default = 67_108_864)]
    pub lru_bytes: u64,
}

#[derive(Debug, Config)]
pub struct SearchConfig {
    /// The heap size in bytes for the in-mem tantivy writer. Larger values
    /// trade RAM for fewer segment merges - the index itself lives in
    /// `RamDirectory` and is rebuilt from hydrant replay on every restart.
    #[config(env = "BOBBIN_SEARCH_HEAP_BYTES", default = 50_000_000)]
    pub heap_bytes: u64,
}

#[derive(Debug, Config)]
pub struct KnotConfig {
    /// Whether to allow the knot proxy to dial private/loopback addresses. Off in
    /// production - on for local testing against a knotserver on localhost.
    #[config(env = "BOBBIN_KNOT_ALLOW_PRIVATE", default = false)]
    pub allow_private: bool,

    /// Require https on knot hosts. Disable only when proxying to a local
    /// knot for development.
    #[config(env = "BOBBIN_KNOT_REQUIRE_HTTPS", default = true)]
    pub require_https: bool,
}

#[derive(Debug, Config)]
pub struct MirrorConfig {
    #[config(env = "BOBBIN_MIRROR_URL")]
    pub url: Option<Url>,
}

#[derive(Debug, Config)]
pub struct LogConfig {
    /// Log emitter format. `text` produces human-readable output for local
    /// development. `json` emits one structured object per line for log
    /// shippers in production.
    #[config(env = "BOBBIN_LOG_FORMAT", default = "text")]
    pub format: String,

    /// `tracing-subscriber` env-filter directive. Defaults to `info` across
    /// every span - override with `BOBBIN_LOG=bobbin_xrpc=debug,info` etc.
    #[config(env = "BOBBIN_LOG", default = "info")]
    pub filter: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogFormat {
    Text,
    Json,
}

impl std::str::FromStr for LogFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            other => Err(format!(
                "log format must be `text` or `json`, got `{other}`"
            )),
        }
    }
}

pub fn load(path: Option<&PathBuf>) -> anyhow::Result<BobbinConfig> {
    check_envs(std::env::vars().map(|(k, _)| k))?;
    if let Some(p) = path {
        check_keys(p)?;
    }
    check_keys(Path::new(SYSTEM_CONFIG_PATH))?;
    let mut builder = BobbinConfig::builder().env();
    if let Some(p) = path {
        builder = builder.file(p);
    }
    let config = builder
        .file(SYSTEM_CONFIG_PATH)
        .load()
        .context("load configuration")?;
    config
        .server
        .trusted_proxies()
        .context("server.trusted_proxies takes a bare IP address or a CIDR block")?;
    Ok(config)
}

pub fn template() -> String {
    confique::toml::template::<BobbinConfig>(confique::toml::FormatOptions::default())
}

fn check_keys(path: &Path) -> anyhow::Result<()> {
    let bytes = match std::fs::read_to_string(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(anyhow!("read {}: {e}", path.display())),
    };
    let value: toml::Value =
        toml::from_str(&bytes).map_err(|e| anyhow!("parse {}: {e}", path.display()))?;
    let known: HashSet<&str> = KNOWN_KEYS.iter().copied().collect();
    let unknown: Vec<String> = collect_paths(&value, "")
        .into_iter()
        .filter(|p| !known.contains(p.as_str()))
        .collect();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "unknown keys in {}: {}\nrun `bobbin config-template` for the canonical schema",
            path.display(),
            unknown.join(", "),
        ))
    }
}

fn check_envs<I, S>(iter: I) -> anyhow::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let known: HashSet<&str> = KNOWN_ENVS.iter().copied().collect();
    let unknown: Vec<String> = iter
        .into_iter()
        .filter_map(|var| {
            let var = var.as_ref();
            (var.starts_with(ENV_PREFIX) && !known.contains(var)).then(|| var.to_owned())
        })
        .collect();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "unknown {ENV_PREFIX}* environment variables: {}\nrun `bobbin config-template` for the canonical schema",
            unknown.join(", "),
        ))
    }
}

fn collect_paths(value: &toml::Value, prefix: &str) -> Vec<String> {
    match value {
        toml::Value::Table(table) => table
            .iter()
            .flat_map(|(key, child)| {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                match child {
                    toml::Value::Table(_) => collect_paths(child, &path),
                    _ => vec![path],
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
fn template_paths(template: &str) -> Vec<String> {
    template
        .lines()
        .scan(String::new(), |section, line| {
            let trimmed = line.trim_start();
            if let Some(name) = trimmed
                .strip_prefix('[')
                .and_then(|rest| rest.trim_end().strip_suffix(']'))
            {
                *section = name.to_owned();
                return Some(None);
            }
            let path = trimmed
                .strip_prefix('#')
                .and_then(|rest| rest.split_once('='))
                .map(|(key, _)| key.trim())
                .filter(|key| {
                    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                })
                .map(|key| format!("{section}.{key}"));
            Some(path)
        })
        .flatten()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bobbin-config-test-{}-{}",
            std::process::id(),
            name
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn known_keys_exactly_match_template_paths() {
        let template = template();
        let from_template: HashSet<String> = template_paths(&template).into_iter().collect();
        let known: HashSet<String> = KNOWN_KEYS.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(
            from_template, known,
            "KNOWN_KEYS must equal the set of paths the confique template generates",
        );
    }

    #[test]
    fn template_paths_extracts_commented_leaf_assignments_under_each_section() {
        let raw = "[server]\n# Doc comment with = inside.\n#binds = [\"127.0.0.1:8090\"]\n#shutdown_grace_secs = 30\n\n[log]\n#format = \"text\"\n";
        let mut paths = template_paths(raw);
        paths.sort();
        assert_eq!(
            paths,
            vec![
                "log.format".to_owned(),
                "server.binds".to_owned(),
                "server.shutdown_grace_secs".to_owned(),
            ],
        );
    }

    #[test]
    fn unknown_section_rejected() {
        let path = write(
            "unknown_section",
            "[server]\nbinds = [\"127.0.0.1:9000\"]\n\n[mystery]\nfoo = 1\n",
        );
        let err = check_keys(&path).expect_err("must reject");
        assert!(err.to_string().contains("mystery.foo"), "got {err}");
    }

    #[test]
    fn unknown_field_rejected() {
        let path = write("typo", "[server]\nbidns = [\"127.0.0.1:9000\"]\n");
        let err = check_keys(&path).expect_err("must reject");
        assert!(err.to_string().contains("server.bidns"), "got {err}");
    }

    #[test]
    fn missing_file_passes_silently() {
        check_keys(Path::new("/definitely/does/not/exist.toml")).expect("no file is fine");
    }

    #[test]
    fn full_known_config_passes() {
        let path = write("ok", &template());
        check_keys(&path).expect("template must validate against itself");
    }

    #[test]
    fn known_envs_carry_the_env_prefix() {
        KNOWN_ENVS.iter().for_each(|name| {
            assert!(
                name.starts_with(ENV_PREFIX),
                "KNOWN_ENVS entry {name:?} must start with {ENV_PREFIX:?}"
            );
        });
    }

    #[test]
    fn known_envs_matches_every_env_attribute_this_crate_declares() {
        let known: HashSet<&str> = KNOWN_ENVS.iter().copied().collect();
        let declared: HashSet<&str> = [include_str!("config.rs"), include_str!("main.rs")]
            .into_iter()
            .flat_map(|source| {
                source
                    .split("env = \"")
                    .skip(1)
                    .filter_map(|rest| rest.split('"').next())
            })
            .collect();
        assert!(
            declared.contains("BOBBIN_BIND") && declared.contains("BOBBIN_CONFIG"),
            "the scan stopped matching config.rs or main.rs and every name in it would pass unchecked, since it came back with {declared:?}"
        );
        let missing: Vec<&&str> = declared.difference(&known).collect();
        assert!(
            missing.is_empty(),
            "confique reads {missing:?} but check_envs will refuse to start with them set. Add them to KNOWN_ENVS"
        );
        let stale: Vec<&&str> = known.difference(&declared).collect();
        assert!(
            stale.is_empty(),
            "KNOWN_ENVS lists {stale:?}, which the fields stopped reading. Drop them, or check_envs will keep accepting a name that stopped meaning anything"
        );
    }

    #[test]
    fn unknown_bobbin_env_rejected() {
        let err = check_envs(["BOBBIN_BIDNS"]).expect_err("typo must surface");
        assert!(err.to_string().contains("BOBBIN_BIDNS"), "got {err}");
    }

    #[test]
    fn known_bobbin_env_passes() {
        check_envs(["BOBBIN_BIND", "BOBBIN_LOG"]).expect("known names must pass");
    }

    #[test]
    fn non_bobbin_env_ignored() {
        check_envs(["PATH", "HOME", "RUST_LOG"]).expect("only BOBBIN_* names are validated");
    }
}
