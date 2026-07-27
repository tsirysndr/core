use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::TryStreamExt;
use http::{HeaderMap, Method, StatusCode};
use url::{Host, Url};

#[derive(Debug, Clone, thiserror::Error)]
pub enum NetworkError {
    #[error("build: {0}")]
    Build(String),
    #[error("connect: {0}")]
    Connect(String),
    #[error("timeout: {0}")]
    Timeout(String),
    #[error("request: {0}")]
    Request(String),
    #[error("body: {0}")]
    Body(String),
    #[error("response exceeds {limit} bytes")]
    TooLarge { limit: u64 },
    #[error("refusing to reach non-public address {host}")]
    Blocked { host: String },
}

pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => match embedded_ipv4(v6) {
            Some(embedded) => is_blocked_v4(embedded),
            None => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_multicast()
                    || (v6.segments()[0] & 0xfe00) == 0xfc00
                    || (v6.segments()[0] & 0xffc0) == 0xfe80
            }
        },
    }
}

fn is_blocked_v4(v4: Ipv4Addr) -> bool {
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_documentation()
        || v4.is_multicast()
        || v4.octets()[0] == 0
        || v4.octets()[0] >= 240
        || matches!(v4.octets(), [100, second, ..] if (64..=127).contains(&second))
        || matches!(v4.octets(), [198, second, ..] if (18..=19).contains(&second))
}

fn embedded_ipv4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(mapped) = v6.to_ipv4() {
        return Some(mapped);
    }
    let segments = v6.segments();
    if segments[0] == 0x2002 {
        return Some(Ipv4Addr::new(
            (segments[1] >> 8) as u8,
            segments[1] as u8,
            (segments[2] >> 8) as u8,
            segments[2] as u8,
        ));
    }
    if segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2..6] == [0, 0, 0, 0] {
        return Some(Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        ));
    }
    None
}

struct GuardedResolver;

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let resolved = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let allowed: Vec<SocketAddr> =
                resolved.filter(|addr| !is_blocked_ip(addr.ip())).collect();
            if allowed.is_empty() {
                return Err(Box::<dyn std::error::Error + Send + Sync>::from(format!(
                    "{host} resolves only to non-public addresses"
                )));
            }
            Ok(Box::new(allowed.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct HttpLimits {
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub request_timeout: Duration,
    pub max_response_bytes: u64,
    pub block_private_addresses: bool,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            read_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(60),
            max_response_bytes: 16 * 1024 * 1024,
            block_private_addresses: true,
        }
    }
}

pub struct HttpRequest {
    pub method: Method,
    pub url: Url,
    pub headers: HeaderMap,
    pub body: Option<Bytes>,
}

impl HttpRequest {
    pub fn get(url: Url) -> Self {
        Self {
            method: Method::GET,
            url,
            headers: HeaderMap::new(),
            body: None,
        }
    }

    pub fn post(url: Url, body: Bytes) -> Self {
        Self {
            method: Method::POST,
            url,
            headers: HeaderMap::new(),
            body: Some(body),
        }
    }
}

#[derive(Debug)]
pub struct HttpResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

pub type HttpFuture = Pin<Box<dyn Future<Output = Result<HttpResponse, NetworkError>> + Send>>;

pub type ByteStream = Pin<Box<dyn futures::Stream<Item = Result<Bytes, NetworkError>> + Send>>;

pub struct StreamedResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: ByteStream,
}

pub type StreamFuture =
    Pin<Box<dyn Future<Output = Result<StreamedResponse, NetworkError>> + Send>>;

pub trait HttpTransport: Send + Sync + 'static {
    fn execute(&self, request: HttpRequest) -> HttpFuture;

    fn execute_streamed(&self, request: HttpRequest) -> StreamFuture {
        let response = self.execute(request);
        Box::pin(async move {
            let response = response.await?;
            Ok(StreamedResponse {
                status: response.status,
                headers: response.headers,
                body: Box::pin(futures::stream::once(std::future::ready(Ok(response.body)))),
            })
        })
    }
}

pub struct ReqwestHttp {
    client: reqwest::Client,
    max_response_bytes: u64,
    block_private_addresses: bool,
}

impl ReqwestHttp {
    pub fn new(limits: HttpLimits) -> Result<Self, NetworkError> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(limits.connect_timeout)
            .read_timeout(limits.read_timeout)
            .timeout(limits.request_timeout)
            .redirect(reqwest::redirect::Policy::none());
        if limits.block_private_addresses {
            builder = builder.dns_resolver(Arc::new(GuardedResolver));
        }
        if let Some(path) = std::env::var_os("KNOT_EXTRA_CA_FILE") {
            let pem =
                std::fs::read(&path).map_err(|error| NetworkError::Build(error.to_string()))?;
            builder = reqwest::Certificate::from_pem_bundle(&pem)
                .map_err(|error| NetworkError::Build(error.to_string()))?
                .into_iter()
                .fold(builder, reqwest::ClientBuilder::add_root_certificate);
        }
        let client = builder
            .build()
            .map_err(|error| NetworkError::Build(error.to_string()))?;
        Ok(Self {
            client,
            max_response_bytes: limits.max_response_bytes,
            block_private_addresses: limits.block_private_addresses,
        })
    }
}

impl HttpTransport for ReqwestHttp {
    fn execute(&self, request: HttpRequest) -> HttpFuture {
        let client = self.client.clone();
        let limit = self.max_response_bytes;
        let guard = self.block_private_addresses;
        Box::pin(async move {
            if let Some(host) = guard.then(|| blocked_literal(&request.url)).flatten() {
                return Err(NetworkError::Blocked { host });
            }
            let mut builder = client
                .request(request.method, request.url)
                .headers(request.headers);
            if let Some(body) = request.body {
                builder = builder.body(body);
            }
            let response = builder.send().await.map_err(map_reqwest)?;
            let status = response.status();
            let headers = response.headers().clone();
            if response.content_length().is_some_and(|len| len > limit) {
                return Err(NetworkError::TooLarge { limit });
            }
            let body = bounded_body(response, limit).await?;
            Ok(HttpResponse {
                status,
                headers,
                body,
            })
        })
    }

    fn execute_streamed(&self, request: HttpRequest) -> StreamFuture {
        let client = self.client.clone();
        let guard = self.block_private_addresses;
        Box::pin(async move {
            if let Some(host) = guard.then(|| blocked_literal(&request.url)).flatten() {
                return Err(NetworkError::Blocked { host });
            }
            let mut builder = client
                .request(request.method, request.url)
                .headers(request.headers);
            if let Some(body) = request.body {
                builder = builder.body(body);
            }
            let response = builder.send().await.map_err(map_reqwest)?;
            let status = response.status();
            let headers = response.headers().clone();
            let body: ByteStream = Box::pin(response.bytes_stream().map_err(|error| {
                if error.is_timeout() {
                    NetworkError::Timeout(error.to_string())
                } else {
                    NetworkError::Body(error.to_string())
                }
            }));
            Ok(StreamedResponse {
                status,
                headers,
                body,
            })
        })
    }
}

async fn bounded_body(response: reqwest::Response, limit: u64) -> Result<Bytes, NetworkError> {
    response
        .bytes_stream()
        .map_err(|error| {
            if error.is_timeout() {
                NetworkError::Timeout(error.to_string())
            } else {
                NetworkError::Body(error.to_string())
            }
        })
        .try_fold(Vec::new(), |mut buffer, chunk| async move {
            if buffer.len() as u64 + chunk.len() as u64 > limit {
                return Err(NetworkError::TooLarge { limit });
            }
            buffer.extend_from_slice(&chunk);
            Ok(buffer)
        })
        .await
        .map(Bytes::from)
}

fn blocked_literal(url: &Url) -> Option<String> {
    match url.host()? {
        Host::Ipv4(ip) if is_blocked_ip(IpAddr::V4(ip)) => Some(ip.to_string()),
        Host::Ipv6(ip) if is_blocked_ip(IpAddr::V6(ip)) => Some(ip.to_string()),
        _ => None,
    }
}

fn map_reqwest(error: reqwest::Error) -> NetworkError {
    if error.is_timeout() {
        NetworkError::Timeout(error.to_string())
    } else if error.is_connect() {
        NetworkError::Connect(error.to_string())
    } else {
        NetworkError::Request(error.to_string())
    }
}

pub struct FakeHttp<F> {
    responder: F,
}

impl<F> FakeHttp<F>
where
    F: Fn(&HttpRequest) -> Result<HttpResponse, NetworkError> + Send + Sync + 'static,
{
    pub fn new(responder: F) -> Self {
        Self { responder }
    }
}

impl<F> HttpTransport for FakeHttp<F>
where
    F: Fn(&HttpRequest) -> Result<HttpResponse, NetworkError> + Send + Sync + 'static,
{
    fn execute(&self, request: HttpRequest) -> HttpFuture {
        let result = (self.responder)(&request);
        Box::pin(async move { result })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};

    #[test]
    fn blocked_addresses_cover_the_internal_ranges() {
        let blocked = [
            "127.0.0.1",
            "10.0.0.5",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "198.18.0.1",
            "240.0.0.1",
            "0.0.0.0",
            "::1",
            "::ffff:127.0.0.1",
            "fd00::1",
            "fe80::1",
            "2002:7f00:1::",
            "64:ff9b::7f00:1",
        ];
        for raw in blocked {
            assert!(
                is_blocked_ip(raw.parse().unwrap()),
                "{raw} should be blocked"
            );
        }
        let allowed = [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "2606:4700:4700::1111",
            "2002:808:808::",
            "64:ff9b::808:808",
        ];
        for raw in allowed {
            assert!(
                !is_blocked_ip(raw.parse().unwrap()),
                "{raw} should be allowed"
            );
        }
    }

    fn tiny_limits(max_response_bytes: u64, request_timeout: Duration) -> HttpLimits {
        HttpLimits {
            connect_timeout: Duration::from_millis(200),
            read_timeout: Duration::from_millis(200),
            request_timeout,
            max_response_bytes,
            block_private_addresses: false,
        }
    }

    fn serve_body(body: Vec<u8>, with_content_length: bool) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.read(&mut [0u8; 1024]);
                let header = if with_content_length {
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len())
                } else {
                    "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_string()
                };
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        addr
    }

    fn serve_hang() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.read(&mut [0u8; 1024]);
                std::thread::sleep(Duration::from_secs(5));
                drop(stream);
            }
        });
        addr
    }

    async fn fetch(addr: SocketAddr, limits: HttpLimits) -> Result<HttpResponse, NetworkError> {
        let transport = ReqwestHttp::new(limits).expect("client builds");
        let url = Url::parse(&format!("http://{addr}/")).expect("url");
        transport.execute(HttpRequest::get(url)).await
    }

    #[tokio::test]
    async fn oversized_response_is_rejected_whether_declared_or_streamed() {
        futures::stream::iter([true, false])
            .for_each(|with_content_length| async move {
                let addr = serve_body(vec![0u8; 4096], with_content_length);
                let result = fetch(addr, tiny_limits(64, Duration::from_secs(2))).await;
                assert!(matches!(result, Err(NetworkError::TooLarge { limit: 64 })));
            })
            .await;
    }

    #[tokio::test]
    async fn small_response_within_limit_succeeds() {
        let addr = serve_body(b"pong".to_vec(), true);
        let response = fetch(addr, tiny_limits(64, Duration::from_secs(2)))
            .await
            .expect("response within limit");
        assert_eq!(response.body.as_ref(), b"pong");
    }

    #[tokio::test]
    async fn unresponsive_server_times_out() {
        let addr = serve_hang();
        let result = fetch(addr, tiny_limits(1024, Duration::from_millis(150))).await;
        assert!(matches!(result, Err(NetworkError::Timeout(_))));
    }

    #[tokio::test]
    async fn connect_failure_surfaces_typed_error() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let result = fetch(addr, tiny_limits(1024, Duration::from_secs(2))).await;
        assert!(matches!(
            result,
            Err(NetworkError::Connect(_) | NetworkError::Request(_) | NetworkError::Timeout(_))
        ));
    }

    #[test]
    fn fake_http_returns_canned_response() {
        let transport = FakeHttp::new(|request: &HttpRequest| {
            assert_eq!(request.method, Method::GET);
            Ok(HttpResponse {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                body: Bytes::from_static(b"pong"),
            })
        });
        let request = HttpRequest::get(Url::parse("https://oyster.cafe/ping").unwrap());
        let response =
            futures::executor::block_on(transport.execute(request)).expect("fake response");
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.body.as_ref(), b"pong");
    }
}
