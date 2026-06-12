use crate::host_proxy::VsockTcpProxy;
use anyhow::Result;

pub struct WriteCacheProxy {
    inner: VsockTcpProxy,
}

impl WriteCacheProxy {
    pub async fn start(host_cid: u32, host_port: u32) -> Result<Option<Self>> {
        if host_port == 0 {
            return Ok(None);
        }

        let inner =
            VsockTcpProxy::start("write cache proxy", "127.0.0.1:0", host_cid, host_port).await?;
        Ok(Some(Self { inner }))
    }

    pub fn url(&self) -> &str {
        self.inner.url()
    }
}
