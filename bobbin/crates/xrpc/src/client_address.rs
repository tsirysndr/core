use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::OnceLock;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use trusted_proxies::TrustedProxies;

pub(crate) static X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");

#[derive(Default)]
pub struct ClientAddress {
    proxies: TrustedProxies,
    ignored_header: OnceLock<IpAddr>,
    no_socket: OnceLock<()>,
}

impl ClientAddress {
    pub fn new(proxies: TrustedProxies) -> Self {
        Self {
            proxies,
            ..Self::default()
        }
    }

    pub(crate) fn of(&self, headers: &HeaderMap, socket: SocketPeer) -> Option<HeaderValue> {
        match socket.0 {
            None => {
                if self.no_socket.set(()).is_ok() {
                    tracing::warn!(
                        "bobbin won't forward a client address to the knot for this request, and every client will share one rate-limit bucket there, because bobbin doesn't have a socket address for it. Serve the listener with `into_make_service_with_connect_info`. This warning reports the first such request only."
                    );
                }
                None
            }
            Some(peer) => {
                let relays = self.proxies.contains(peer);
                if headers.contains_key(&X_FORWARDED_FOR)
                    && !relays
                    && self.ignored_header.set(peer).is_ok()
                {
                    tracing::warn!(
                        %peer,
                        "bobbin ignored x-forwarded-for and will forward the address this peer connected from, because the peer is outside server.trusted_proxies. Add this address to server.trusted_proxies if it's the reverse proxy, or every client it serves will share one rate-limit bucket on each knot. This warning reports the first such peer only."
                    );
                }
                let client = relays
                    .then(|| {
                        self.proxies.rightmost_untrusted(
                            headers
                                .get_all(&X_FORWARDED_FOR)
                                .iter()
                                .filter_map(|value| value.to_str().ok())
                                .flat_map(|value| value.split(',')),
                        )
                    })
                    .flatten()
                    .unwrap_or_else(|| peer.to_canonical());
                HeaderValue::try_from(client.to_string()).ok()
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SocketPeer(Option<IpAddr>);

impl<S: Send + Sync> FromRequestParts<S> for SocketPeer {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|info| info.0.ip()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }

    fn relaying<'a>(entries: impl IntoIterator<Item = &'a str>) -> ClientAddress {
        ClientAddress::new(TrustedProxies::parse(entries).unwrap())
    }

    fn chain(value: Option<&str>) -> HeaderMap {
        value
            .map(|value| {
                let mut map = HeaderMap::new();
                map.insert(&X_FORWARDED_FOR, value.parse().unwrap());
                map
            })
            .unwrap_or_default()
    }

    fn forwarded(proxies: &[&str], socket: Option<&str>, claimed: Option<&str>) -> Option<String> {
        relaying(proxies.iter().copied())
            .of(&chain(claimed), SocketPeer(socket.map(ip)))
            .map(|value| value.to_str().unwrap().to_owned())
    }

    #[test]
    fn bobbin_forwards_the_socket_unless_a_listed_proxy_relayed_the_request() {
        let listed: &[&str] = &["127.0.0.1", "173.245.48.0/20"];
        [
            (&["127.0.0.1"][..], Some("203.0.113.7"), Some("198.51.100.4"), Some("203.0.113.7"),
             "bobbin must answer for the socket, since a client reaching it directly wrote that header itself"),
            (&[], Some("203.0.113.7"), Some("198.51.100.4"), Some("203.0.113.7"),
             "an operator who hasn't configured a proxy will get the socket, since honoring the header by default would hand every client its own rate-limit bucket on every knot downstream. The knot reads its own empty list the opposite way, as trusting every peer, so don't carry either default across"),
            (listed, Some("127.0.0.1"), Some("198.51.100.4"), Some("198.51.100.4"),
             "a listed proxy hands over the address it recorded"),
            (listed, Some("127.0.0.1"), Some("198.51.100.4, 173.245.48.9"), Some("198.51.100.4"),
             "and a second listed hop is stepped over with it"),
            (listed, Some("127.0.0.1"), Some("  198.51.100.4  "), Some("198.51.100.4"),
             "padding around an entry won't hide it"),
            (listed, Some("127.0.0.1"), Some("203.0.113.7, 198.51.100.4"), Some("198.51.100.4"),
             "bobbin takes the rightmost unlisted hop"),
            (listed, Some("127.0.0.1"), None, Some("127.0.0.1"),
             "a listed proxy that didn't send the header leaves its own socket to forward"),
            (listed, Some("127.0.0.1"), Some("not-an-ip"), Some("127.0.0.1"),
             "bobbin stops at an entry it can't parse and forwards the socket, because a client can write anything left of the proxy"),
            (listed, Some("127.0.0.1"), Some("127.0.0.1"), Some("127.0.0.1"),
             "a chain of listed hops alone leaves the proxy's socket too"),
            (&["173.245.48.0/20"], Some("173.245.48.9"), Some("198.51.100.4, 203.0.113.7"), Some("203.0.113.7"),
             "bobbin stops before reaching anything a client wrote, since the proxy appends the address it saw to the right of all of it"),
            (&["127.0.0.1"], Some("::ffff:203.0.113.7"), None, Some("203.0.113.7"),
             "a v4-mapped socket and the plain address are one client"),
            (&["127.0.0.1"], Some("::ffff:127.0.0.1"), Some("::ffff:198.51.100.4"), Some("198.51.100.4"),
             "both spellings must key to one bucket, since a dual-stack listener reports the mapped form on both sides"),
            (&["127.0.0.1"], None, Some("198.51.100.4"), None,
             "bobbin won't identify a client it hasn't seen connect, since it can't check the header against anything"),
        ]
        .iter()
        .for_each(|&(proxies, socket, claimed, expected, why)| {
            assert_eq!(
                forwarded(proxies, socket, claimed).as_deref(),
                expected,
                "{why}: {proxies:?} saw {socket:?} claiming {claimed:?}"
            );
        });
    }
}
