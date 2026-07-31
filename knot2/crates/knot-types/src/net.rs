use std::net::IpAddr;

use http::{HeaderMap, HeaderName};
pub use trusted_proxies::{ProxyNetError, TrustedProxies, comma_separated};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerKey {
    Relayed(IpAddr),
    Socket(IpAddr),
    SocketWithIgnoredHeader(IpAddr),
    Unidentified,
}

impl PeerKey {
    pub fn address(self) -> Option<IpAddr> {
        match self {
            Self::Relayed(peer) | Self::Socket(peer) | Self::SocketWithIgnoredHeader(peer) => {
                Some(peer)
            }
            Self::Unidentified => None,
        }
    }

    pub fn ignored_header(self) -> Option<IpAddr> {
        match self {
            Self::SocketWithIgnoredHeader(peer) => Some(peer),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProxyTrust {
    header: Option<HeaderName>,
    proxies: TrustedProxies,
}

impl ProxyTrust {
    pub fn new(header: Option<HeaderName>, proxies: TrustedProxies) -> Self {
        Self { header, proxies }
    }

    pub fn trusts_any_peer(&self) -> bool {
        self.header.is_some() && self.proxies.is_empty()
    }

    fn trusts(&self, socket: Option<IpAddr>) -> bool {
        match socket {
            _ if self.proxies.is_empty() => true,
            Some(socket) => self.proxies.contains(socket),
            None => false,
        }
    }

    pub fn peer_key(&self, headers: &HeaderMap, socket: Option<IpAddr>) -> PeerKey {
        match (self.relayed_peer(headers, socket), socket) {
            (Some(relayed), _) => PeerKey::Relayed(relayed.to_canonical()),
            (None, None) => PeerKey::Unidentified,
            (None, Some(socket)) => match self.ignores_header_from(headers, socket) {
                true => PeerKey::SocketWithIgnoredHeader(socket.to_canonical()),
                false => PeerKey::Socket(socket.to_canonical()),
            },
        }
    }

    pub fn client_peer(&self, headers: &HeaderMap, socket: Option<IpAddr>) -> Option<IpAddr> {
        self.peer_key(headers, socket).address()
    }

    pub fn client_peer_of(&self, headers: &HeaderMap, socket: IpAddr) -> IpAddr {
        self.peer_key(headers, Some(socket))
            .address()
            .unwrap_or(socket.to_canonical())
    }

    fn relayed_peer(&self, headers: &HeaderMap, socket: Option<IpAddr>) -> Option<IpAddr> {
        self.header
            .as_ref()
            .filter(|_| self.trusts(socket))
            .and_then(|header| {
                self.proxies.rightmost_untrusted(
                    headers
                        .get_all(header)
                        .iter()
                        .filter_map(|value| value.to_str().ok())
                        .flat_map(|value| value.split(',')),
                )
            })
    }

    fn ignores_header_from(&self, headers: &HeaderMap, socket: IpAddr) -> bool {
        self.header
            .as_ref()
            .is_some_and(|header| headers.contains_key(header))
            && !self.trusts(Some(socket))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(value: Option<&str>) -> HeaderMap {
        value
            .map(|value| {
                let mut map = HeaderMap::new();
                map.insert(forwarded_for(), value.parse().unwrap());
                map
            })
            .unwrap_or_default()
    }

    fn forwarded_for() -> HeaderName {
        HeaderName::from_static("x-forwarded-for")
    }

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }

    fn trusting<'a>(entries: impl IntoIterator<Item = &'a str>) -> TrustedProxies {
        TrustedProxies::parse(entries).unwrap()
    }

    fn relaying<'a>(entries: impl IntoIterator<Item = &'a str>) -> ProxyTrust {
        ProxyTrust::new(Some(forwarded_for()), trusting(entries))
    }

    #[test]
    fn the_knot_reads_the_rightmost_entry_from_any_peer_with_an_empty_allowlist() {
        let anyone = ProxyTrust::new(Some(forwarded_for()), TrustedProxies::default());
        [
            (Some("203.0.113.7, 198.51.100.4"), "198.51.100.4"),
            (Some("  192.0.2.1  "), "192.0.2.1"),
            (Some("not-an-ip"), "192.0.2.9"),
            (None, "192.0.2.9"),
        ]
        .iter()
        .for_each(|&(header, expected)| {
            assert_eq!(
                anyone.client_peer_of(&headers(header), ip("192.0.2.9")),
                ip(expected),
                "{header:?}"
            );
        });
    }

    #[test]
    fn the_knot_joins_every_line_of_a_repeated_header_into_one_chain() {
        let mut map = HeaderMap::new();
        map.append(forwarded_for(), "203.0.113.7".parse().unwrap());
        map.append(forwarded_for(), "198.51.100.4".parse().unwrap());
        assert_eq!(
            relaying(["127.0.0.1"]).client_peer(&map, Some(ip("127.0.0.1"))),
            Some(ip("198.51.100.4")),
            "a proxy that appends a second header line puts the address we want in the last line"
        );
    }

    #[test]
    fn the_header_applies_only_to_a_peer_on_the_allowlist() {
        let relayed = headers(Some("198.51.100.4"));
        [
            (&["127.0.0.1"][..], "127.0.0.1", "198.51.100.4",
             "a request relayed by the listed proxy is limited by the address the proxy recorded"),
            (&["127.0.0.1"], "203.0.113.7", "203.0.113.7",
             "a client reaching the knot directly forged the header and must answer for its socket"),
            (&["127.0.0.1", "::1"], "::1", "198.51.100.4",
             "a second listed entry relays as readily as the first"),
            (&["127.0.0.1"], "::ffff:127.0.0.1", "198.51.100.4",
             "binding [::] turns an IPv4 proxy into ::ffff:127.0.0.1 and the allowlist must still match it"),
            (&["::ffff:127.0.0.1"], "127.0.0.1", "198.51.100.4",
             "an operator who writes the mapped form must match a plain IPv4 peer too"),
            (&["127.0.0.1"], "::1",  "::1",
             "that peer answers for the socket it connected from, since the IPv6 loopback is a different address from the IPv4 loopback"),
            (&["127.0.0.1"], "::ffff:203.0.113.7", "203.0.113.7",
             "an ignored header still keys the peer on its socket, canonical so the warning and the bucket agree"),
        ]
        .iter()
        .for_each(|&(listed, socket, expected, why)| {
            assert_eq!(
                relaying(listed.iter().copied()).client_peer(&relayed, Some(ip(socket))),
                Some(ip(expected)),
                "{why}: {listed:?} saw {socket}"
            );
        });
        assert_eq!(
            relaying(["127.0.0.1"]).client_peer(
                &headers(Some("203.0.113.7, not-an-ip")),
                Some(ip("127.0.0.1"))
            ),
            Some(ip("127.0.0.1")),
            "the knot keys on the listed proxy's own socket when it can't read past the chain"
        );
    }

    #[test]
    fn a_caller_with_a_socket_address_gets_the_same_answer_without_an_option() {
        let listed = trusting(["127.0.0.1"]);
        [
            (ProxyTrust::default(), Some("198.51.100.4")),
            (
                ProxyTrust::new(Some(forwarded_for()), listed),
                Some("198.51.100.4"),
            ),
            (
                ProxyTrust::new(Some(forwarded_for()), TrustedProxies::default()),
                None,
            ),
        ]
        .into_iter()
        .for_each(|(trust, value)| {
            let headers = headers(value);
            [ip("127.0.0.1"), ip("203.0.113.7"), ip("::ffff:203.0.113.7")]
                .into_iter()
                .for_each(|socket| {
                    assert_eq!(
                        Some(trust.client_peer_of(&headers, socket)),
                        trust.client_peer(&headers, Some(socket)),
                        "{trust:?} disagreed with itself for {socket} and header {value:?}"
                    );
                });
        });
    }

    #[test]
    fn client_peer_falls_back_to_the_socket_whenever_the_header_doesnt_apply() {
        let socket = ip("203.0.113.7");
        [
            (None, Some("198.51.100.4")),
            (Some(forwarded_for()), None),
            (Some(forwarded_for()), Some("not-an-ip")),
        ]
        .into_iter()
        .for_each(|(header_name, header_value)| {
            let trust = ProxyTrust::new(header_name.clone(), TrustedProxies::default());
            assert_eq!(
                trust.client_peer(&headers(header_value), Some(socket)),
                Some(socket),
                "{header_name:?} with {header_value:?}"
            );
            assert_eq!(
                trust.client_peer(&headers(header_value), Some(ip("::ffff:203.0.113.7"))),
                Some(socket),
                "a v4-mapped socket and the plain v4 address are one client, so they share a key"
            );
        });
    }

    #[test]
    fn the_peer_key_separates_an_ignored_header_from_a_request_that_never_sent_one() {
        let listed = ProxyTrust::new(Some(forwarded_for()), trusting(["127.0.0.1"]));
        assert_eq!(
            listed.peer_key(&headers(Some("198.51.100.4")), Some(ip("203.0.113.7"))),
            PeerKey::SocketWithIgnoredHeader(ip("203.0.113.7")),
            "an operator has to see the address of an unlisted peer that sent the header"
        );
        assert_eq!(
            listed.peer_key(&headers(None), Some(ip("203.0.113.7"))),
            PeerKey::Socket(ip("203.0.113.7")),
            "the allowlist stays untested when a request arrives without the header"
        );
        assert_eq!(
            listed.peer_key(&headers(Some("198.51.100.4")), Some(ip("127.0.0.1"))),
            PeerKey::Relayed(ip("198.51.100.4")),
            "the listed proxy relayed this one"
        );
        assert_eq!(
            listed.peer_key(&headers(Some("198.51.100.4")), None),
            PeerKey::Unidentified
        );
        assert_eq!(
            listed.client_peer(&headers(Some("198.51.100.4")), None),
            None,
            "the knot won't key on a client until it has a socket to check against the allowlist"
        );
    }

    #[test]
    fn only_an_ignored_header_has_an_address_to_warn_about() {
        assert_eq!(
            PeerKey::SocketWithIgnoredHeader(ip("203.0.113.7")).ignored_header(),
            Some(ip("203.0.113.7"))
        );
        [
            PeerKey::Relayed(ip("198.51.100.4")),
            PeerKey::Socket(ip("203.0.113.7")),
            PeerKey::Unidentified,
        ]
        .into_iter()
        .for_each(|key| {
            assert_eq!(
                key.ignored_header(),
                None,
                "{key:?} isn't a misconfigured allowlist"
            );
        });
    }

    #[test]
    fn only_a_header_without_an_allowlist_makes_the_knot_trust_any_peer() {
        let listed = trusting(["127.0.0.1"]);
        assert!(
            ProxyTrust::new(Some(forwarded_for()), TrustedProxies::default()).trusts_any_peer()
        );
        assert!(!ProxyTrust::new(Some(forwarded_for()), listed).trusts_any_peer());
        assert!(!ProxyTrust::default().trusts_any_peer());
    }
}
