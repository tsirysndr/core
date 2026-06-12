use anyhow::{Context, Result};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::task::{JoinError, JoinHandle, JoinSet};
use tokio_vsock::{VsockAddr, VsockStream};
use tracing::{info, warn};

const DEFAULT_DNS_PROXY_ADDR: &str = "127.0.0.1:53";
const SHUTTLE_DNS_PROXY_ADDR_ENV: &str = "SHUTTLE_DNS_PROXY_ADDR";

const MAX_DNS_MESSAGE_BYTES: usize = u16::MAX as usize;
const DNS_IO_TIMEOUT: Duration = Duration::from_secs(10);
const DNS_TCP_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

// implements a proxy that sends dns requests to the spindle and
// lets the spindle resolve any queries, and streams the response back.
//
// we use this because the way spindle isolates QEMU VMs is unreliable
// to rely on. if we unblock the blackholed-routes for private nameservers
// like slirp one, we risk leaking internal DNS zones, even if the guest
// can't connect to them. there is also potentially DNS rebinding issues.
// and other slirp4netns quirks...
//
// this way we also get to filter the DNS queries very easily, so we can
// make sure we remove everything that would leak a host information.
pub struct DnsProxy {
    handles: Vec<JoinHandle<()>>,
}

#[derive(Clone)]
struct HostDnsClient {
    host_cid: u32,
    host_port: u32,
}

impl DnsProxy {
    pub async fn start(host_cid: u32, host_port: u32) -> Result<Option<Self>> {
        if host_port == 0 {
            return Ok(None);
        }

        let addr = std::env::var(SHUTTLE_DNS_PROXY_ADDR_ENV)
            .unwrap_or_else(|_| DEFAULT_DNS_PROXY_ADDR.to_owned());

        let udp = Arc::new(
            UdpSocket::bind(&addr)
                .await
                .with_context(|| format!("bind dns udp listener {addr}"))?,
        );

        let tcp = TcpListener::bind(&addr)
            .await
            .with_context(|| format!("bind dns tcp listener {addr}"))?;

        let host = HostDnsClient {
            host_cid,
            host_port,
        };

        let handles = vec![
            tokio::spawn(udp_loop(udp, host.clone())),
            tokio::spawn(tcp_loop(tcp, host)),
        ];

        info!(%addr, host_cid, host_port, "dns proxy ready");
        Ok(Some(Self { handles }))
    }
}

impl Drop for DnsProxy {
    fn drop(&mut self) {
        for handle in self.handles.drain(..) {
            handle.abort();
        }
    }
}

impl HostDnsClient {
    async fn query(&self, query: Vec<u8>) -> Result<Vec<u8>> {
        match self.query_once(&query).await {
            Ok(response) => Ok(response),
            Err(first_error) => self.query_once(&query).await.with_context(|| {
                format!("dns host query failed after retry; first error: {first_error:#}")
            }),
        }
    }

    async fn query_once(&self, query: &[u8]) -> Result<Vec<u8>> {
        let addr = VsockAddr::new(self.host_cid, self.host_port);

        let mut host = tokio::time::timeout(DNS_IO_TIMEOUT, VsockStream::connect(addr))
            .await
            .context("dns host connect timed out")?
            .with_context(|| {
                format!(
                    "dial host dns proxy cid={} port={}",
                    self.host_cid, self.host_port
                )
            })?;

        tokio::time::timeout(DNS_IO_TIMEOUT, async {
            write_dns_packet(&mut host, query)
                .await
                .context("write dns query to host")?;

            read_dns_packet(&mut host)
                .await
                .context("read dns response from host")?
                .context("host dns proxy closed without response")
        })
        .await
        .context("dns host query timed out")?
    }
}

async fn udp_loop(socket: Arc<UdpSocket>, host: HostDnsClient) {
    let mut buf = vec![0; MAX_DNS_MESSAGE_BYTES];
    let mut tasks = JoinSet::new();

    loop {
        tokio::select! {
            received = socket.recv_from(&mut buf) => match received {
                Ok((len, peer)) => {
                    let query = buf[..len].to_vec();
                    let socket = socket.clone();
                    let host = host.clone();

                    tasks.spawn(async move {
                        if let Err(error) = handle_udp_query(socket, peer, query, host).await {
                            warn!(%peer, %error, "dns udp query failed");
                        }
                    });
                }
                Err(error) => warn!(%error, "dns udp recv failed"),
            },

            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                log_dns_task_result(result);
            }
        }
    }
}

async fn handle_udp_query(
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    query: Vec<u8>,
    host: HostDnsClient,
) -> Result<()> {
    let response = host.query(query).await?;

    socket
        .send_to(&response, peer)
        .await
        .context("send dns udp response")?;

    Ok(())
}

async fn tcp_loop(listener: TcpListener, host: HostDnsClient) {
    let mut tasks = JoinSet::new();

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((conn, peer)) => {
                    let host = host.clone();

                    tasks.spawn(async move {
                        if let Err(error) = handle_tcp_conn(conn, host).await {
                            warn!(%peer, %error, "dns tcp connection failed");
                        }
                    });
                }
                Err(error) => warn!(%error, "dns tcp accept failed"),
            },

            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                log_dns_task_result(result);
            }
        }
    }
}

async fn handle_tcp_conn(mut tcp: TcpStream, host: HostDnsClient) -> Result<()> {
    loop {
        let query = tokio::time::timeout(DNS_TCP_IDLE_TIMEOUT, read_dns_packet(&mut tcp))
            .await
            .context("dns tcp idle timeout")?
            .context("read dns tcp query")?;

        let Some(query) = query else {
            return Ok(());
        };

        let response = host.query(query).await?;

        write_dns_packet(&mut tcp, &response)
            .await
            .context("write dns tcp response")?;
    }
}

async fn read_dns_packet<R>(reader: &mut R) -> io::Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    let mut len_buf = [0; 2];

    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }

    let len = u16::from_be_bytes(len_buf) as usize;
    if len == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "empty dns packet",
        ));
    }

    let mut packet = vec![0; len];
    reader.read_exact(&mut packet).await?;
    Ok(Some(packet))
}

async fn write_dns_packet<W>(writer: &mut W, packet: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    if packet.is_empty() || packet.len() > MAX_DNS_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid dns packet size {}", packet.len()),
        ));
    }

    writer
        .write_all(&(packet.len() as u16).to_be_bytes())
        .await?;
    writer.write_all(packet).await?;
    writer.flush().await
}

fn log_dns_task_result(result: Result<(), JoinError>) {
    if let Err(error) = result {
        warn!(%error, "dns proxy task failed");
    }
}
