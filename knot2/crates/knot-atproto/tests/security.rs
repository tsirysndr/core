use std::io::{Read, Write};
use std::net::TcpListener;

use knot_runtime::{HttpLimits, HttpRequest, HttpTransport, NetworkError, ReqwestHttp};
use url::Url;

#[tokio::test]
async fn the_transport_refuses_a_loopback_target() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let http = ReqwestHttp::new(HttpLimits::default()).unwrap();
    let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
    let error = http.execute(HttpRequest::get(url)).await.unwrap_err();
    assert!(
        matches!(error, NetworkError::Blocked { .. }),
        "got {error:?}"
    );
    drop(listener);
}

#[tokio::test]
async fn a_hostname_resolving_only_to_loopback_is_refused_at_dns() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let http = ReqwestHttp::new(HttpLimits::default()).unwrap();
    let url = Url::parse(&format!("https://localhost:{port}/")).unwrap();
    let error = http.execute(HttpRequest::get(url)).await.unwrap_err();
    assert!(
        matches!(
            error,
            NetworkError::Connect(_) | NetworkError::Request(_) | NetworkError::Timeout(_)
        ),
        "hostname whose only addresses are loopback must fail to connect, got {error:?}"
    );
    drop(listener);
}

#[tokio::test]
async fn a_redirect_to_an_internal_host_is_not_followed() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.read(&mut [0u8; 1024]);
            let _ = stream.write_all(
                b"HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/\r\nContent-Length: 0\r\n\r\n",
            );
        }
    });
    let limits = HttpLimits {
        block_private_addresses: false,
        ..HttpLimits::default()
    };
    let http = ReqwestHttp::new(limits).unwrap();
    let url = Url::parse(&format!("http://{addr}/")).unwrap();
    let response = http.execute(HttpRequest::get(url)).await.unwrap();
    assert_eq!(
        response.status.as_u16(),
        302,
        "302 is surfaced verbatim, redirect to internal host is never followed"
    );
}
