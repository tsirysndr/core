use crate::command::{self, Spec};
use crate::protocol::v1;
use anyhow::{Context, Result};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::Builder;
use tracing::{info, warn};

pub const SPINDLE_RUN_DIR: &str = "/run/spindle";
pub const SPINDLE_NIX_CONFIG: &str = "/run/spindle/nix.conf";
pub const SPINDLE_CACHE_CONFIG: &str = "/run/spindle/cache.json";
pub const SYSTEMCTL_EXECUTABLE: &str = "/run/current-system/sw/bin/systemctl";

// nix lives in different places depending on the guest OS (NixOS system
// profile vs. plain /usr/local on e.g. alpine)
pub fn nix_executable() -> &'static str {
    static NIX: once_cell::sync::Lazy<&'static str> = once_cell::sync::Lazy::new(|| {
        let paths = [
            "/run/current-system/sw/bin/nix",
            "/usr/local/bin/nix",
            "/usr/bin/nix",
        ];
        for candidate in paths {
            if Path::new(candidate).exists() {
                return candidate;
            }
        }
        "/run/current-system/sw/bin/nix"
    });
    &NIX
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RuntimeCacheConfig {
    pub read_urls: Vec<String>,
    pub trusted_public_keys: Vec<String>,
}

// configures nix daemon with the configuration passed from host
pub async fn configure(init: &v1::Init, read_proxy_url: &str) -> Result<RuntimeCacheConfig> {
    let read_urls = vec![read_proxy_url.to_owned()];
    let cfg = RuntimeCacheConfig {
        read_urls,
        trusted_public_keys: clean_strings(&init.cache_trusted_public_keys),
    };

    if cfg.read_urls.is_empty() && cfg.trusted_public_keys.is_empty() {
        remove_if_exists(SPINDLE_NIX_CONFIG)?;
        remove_if_exists(SPINDLE_CACHE_CONFIG)?;
        return Ok(cfg);
    }

    fs::create_dir_all(SPINDLE_RUN_DIR).with_context(|| format!("create {SPINDLE_RUN_DIR}"))?;

    let cache_json = serde_json::to_vec_pretty(&cfg)?;
    write_file_atomic(SPINDLE_CACHE_CONFIG, &cache_json, 0o600)?;

    let mut nix_conf = String::new();
    if !cfg.read_urls.is_empty() {
        writeln!(
            &mut nix_conf,
            "extra-substituters = {}",
            cfg.read_urls.join(" ")
        )
        .unwrap();
    }
    if !cfg.trusted_public_keys.is_empty() {
        writeln!(
            &mut nix_conf,
            "extra-trusted-public-keys = {}",
            cfg.trusted_public_keys.join(" ")
        )
        .unwrap();
    }

    if nix_conf.is_empty() {
        remove_if_exists(SPINDLE_NIX_CONFIG)?;
        return Ok(cfg);
    }

    write_file_atomic(SPINDLE_NIX_CONFIG, nix_conf.as_bytes(), 0o644)?;
    restart_nix_daemon().await;
    info!(
        read_urls = ?cfg.read_urls,
        trusted_public_keys = cfg.trusted_public_keys.len(),
        "configured nix cache"
    );

    Ok(cfg)
}

pub fn clean_strings(values: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(values.len());

    for value in values {
        let value = value.trim();
        if value.is_empty() || !seen.insert(value.to_owned()) {
            continue;
        }
        out.push(value.to_owned());
    }

    out
}

pub fn clean_store_paths(values: &[String]) -> Vec<String> {
    clean_strings(values)
        .into_iter()
        .filter(|value| value.starts_with("/nix/store/"))
        .collect()
}

pub async fn nix_version() -> String {
    let spec = Spec::new(nix_executable())
        .arg("--version")
        .timeout(Duration::from_secs(1));

    let Ok(output) = command::run_capture(spec).await else {
        return String::new();
    };
    if !output.success() {
        return String::new();
    }

    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn write_file_atomic(path: impl AsRef<Path>, data: &[u8], mode: u32) -> Result<()> {
    let path = path.as_ref();
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let prefix = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| format!(".{name}.tmp-"))
        .unwrap_or_else(|| ".tmp-".to_owned());

    let mut tmp = Builder::new()
        .prefix(&prefix)
        .permissions(fs::Permissions::from_mode(mode))
        .tempfile_in(dir)
        .with_context(|| format!("create temp file for {}", path.display()))?;

    // no separate sync here because we don't need to be crash-safe (this is an
    // ephemeral vm) only atomicity is needed
    tmp.write_all(data)
        .with_context(|| format!("write temp file for {}", path.display()))?;
    tmp.persist(path)
        .map(|_| ())
        .map_err(|err| err.error)
        .with_context(|| format!("install {}", path.display()))
}

const NIX_DAEMON_SOCKET: &str = "/nix/var/nix/daemon-socket/socket";

async fn restart_nix_daemon() {
    if Path::new(SYSTEMCTL_EXECUTABLE).exists() {
        let spec = Spec::new(SYSTEMCTL_EXECUTABLE)
            .args(["try-restart", "nix-daemon.service"])
            .timeout(Duration::from_secs(5));
        match command::run_capture(spec).await {
            Ok(output) if output.success() => {}
            Ok(output) => warn!(
                exit_code = output.exit.exit_code,
                error = ?output.exit.error,
                output = %output.combined_lossy(),
                "nix-daemon restart failed"
            ),
            Err(error) => warn!(%error, "nix-daemon restart failed"),
        }
        return;
    }

    let old_pids = nix_daemon_pids();
    if old_pids.is_empty() {
        info!("no nix-daemon running, skipping restart");
        return;
    }
    for pid in &old_pids {
        if let Err(error) = signal::kill(*pid, Signal::SIGTERM) {
            warn!(%error, pid = pid.as_raw(), "failed to signal nix-daemon");
        }
    }

    // wait for nix daemon to be gone, kill is not sync
    wait_pids_gone(&old_pids, Duration::from_secs(5)).await;
    wait_for_nix_daemon_socket(Duration::from_secs(5)).await;
}

fn nix_daemon_pids() -> Vec<Pid> {
    let self_pid = std::process::id();
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<i32>().ok()?;
            if pid as u32 == self_pid {
                return None;
            }
            let comm = fs::read_to_string(entry.path().join("comm")).ok()?;
            (comm.trim() == "nix-daemon").then(|| Pid::from_raw(pid))
        })
        .collect()
}

async fn wait_pids_gone(pids: &[Pid], timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        // signal 0 only checks for existence; Err (ESRCH) means it's gone
        if !pids.iter().any(|pid| signal::kill(*pid, None).is_ok()) {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            warn!("old nix-daemon did not exit before restart timeout");
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_nix_daemon_socket(timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if tokio::net::UnixStream::connect(NIX_DAEMON_SOCKET)
            .await
            .is_ok()
        {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            warn!(
                socket = NIX_DAEMON_SOCKET,
                "nix-daemon did not come back after restart"
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn remove_if_exists(path: impl AsRef<Path>) -> Result<()> {
    let path: PathBuf = path.as_ref().to_owned();
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}
