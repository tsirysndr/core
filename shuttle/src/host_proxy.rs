use anyhow::{Context, Result};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinError, JoinHandle, JoinSet};
use tokio_vsock::{VsockAddr, VsockStream};
use tracing::{info, warn};

// this implements a vsock <-> tcp proxy for communicating with spindle
pub struct VsockTcpProxy {
    url: String,
    handle: JoinHandle<()>,
}

impl VsockTcpProxy {
    pub async fn start(
        name: &'static str,
        bind_addr: &str,
        host_cid: u32,
        host_port: u32,
    ) -> Result<Self> {
        if host_port == 0 {
            anyhow::bail!("port 0 cant be requested");
        }

        let listener = TcpListener::bind(bind_addr)
            .await
            .with_context(|| format!("bind {name} listener {bind_addr}"))?;
        let local_addr = listener
            .local_addr()
            .with_context(|| format!("{name} local address"))?;
        let url = format!("http://{local_addr}");

        let handle = tokio::spawn(async move {
            accept_loop(name, listener, host_cid, host_port).await;
        });

        info!(%url, host_cid, host_port, "{name} ready");
        Ok(Self { url, handle })
    }

    pub fn url(&self) -> &str {
        &self.url
    }
}

impl Drop for VsockTcpProxy {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn accept_loop(name: &'static str, listener: TcpListener, host_cid: u32, host_port: u32) {
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((conn, _addr)) => {
                    tasks.spawn(async move {
                        if let Err(error) = proxy_conn(name, conn, host_cid, host_port).await {
                            warn!(%error, "{name} connection failed");
                        }
                    });
                }
                Err(error) => warn!(%error, "{name} accept failed"),
            },
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                log_proxy_task_result(result);
            }
        }
    }
}

fn log_proxy_task_result(result: Result<(), JoinError>) {
    if let Err(error) = result {
        warn!(%error, "proxy task failed");
    }
}

async fn proxy_conn(
    name: &'static str,
    mut tcp: TcpStream,
    host_cid: u32,
    host_port: u32,
) -> Result<()> {
    let mut host = VsockStream::connect(VsockAddr::new(host_cid, host_port))
        .await
        .with_context(|| format!("dial host {name} cid={host_cid} port={host_port}"))?;

    tokio::io::copy_bidirectional(&mut tcp, &mut host)
        .await
        .context("proxy connection copy")?;
    Ok(())
}
