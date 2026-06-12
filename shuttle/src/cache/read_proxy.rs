use crate::host_proxy::VsockTcpProxy;
use anyhow::Result;

const DEFAULT_CACHE_READ_PROXY_ADDR: &str = "127.0.0.1:10500";
const SHUTTLE_CACHE_READ_PROXY_ADDR_ENV: &str = "SHUTTLE_CACHE_READ_PROXY_ADDR";

pub struct ReadCacheProxy {
    inner: VsockTcpProxy,
}

impl ReadCacheProxy {
    pub async fn start(host_cid: u32, host_port: u32) -> Result<Option<Self>> {
        if host_port == 0 {
            return Ok(None);
        }

        let addr = std::env::var(SHUTTLE_CACHE_READ_PROXY_ADDR_ENV)
            .unwrap_or_else(|_| DEFAULT_CACHE_READ_PROXY_ADDR.to_owned());

        let inner = VsockTcpProxy::start("read cache proxy", &addr, host_cid, host_port).await?;
        Ok(Some(Self { inner }))
    }

    pub fn url(&self) -> &str {
        self.inner.url()
    }
}
