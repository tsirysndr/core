#![cfg(target_os = "linux")]

mod activation;
mod cache;
mod command;
mod dns_proxy;
mod exec;
mod host_proxy;
mod logging;
mod nix_config;
mod protocol;
mod session;

use std::env;
use std::time::Duration;
use tracing::warn;

#[macro_export]
macro_rules! cfg {
    (@val $key:expr) => {
        std::env::var(concat!("SHUTTLE_", $key))
    };
    ($key:expr, $default:expr) => {
        cfg!(@val $key)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or($default.to_owned())
            .into()
    };
}

fn cmdline_param(key: &str) -> Option<String> {
    let cmdline = std::fs::read_to_string("/proc/cmdline").ok()?;
    cmdline
        .split_whitespace()
        .find_map(|tok| Some(tok.strip_prefix(key)?.strip_prefix('=')?.to_owned()))
}

#[tokio::main]
async fn main() {
    logging::init();

    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) == Some("enqueue-built-paths") {
        cache::enqueue_built_paths(&args[2..]).await;
        return;
    }

    let port: u32 = cfg!(@val "VSOCK_PORT")
        .ok()
        .or_else(|| cmdline_param("shuttle.vsock_port"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(protocol::DEFAULT_PORT);
    let host_cid: u32 = cfg!("HOST_CID", tokio_vsock::VMADDR_CID_HOST);

    loop {
        if let Err(error) = session::run(host_cid, port).await {
            warn!(host_cid, port, %error, "agent session failed");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
