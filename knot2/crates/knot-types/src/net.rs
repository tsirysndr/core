use std::collections::BTreeSet;
use std::net::IpAddr;

use http::{HeaderMap, HeaderName};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustedProxies(BTreeSet<IpAddr>);

impl TrustedProxies {
    pub fn new(addresses: impl IntoIterator<Item = IpAddr>) -> Self {
        Self(
            addresses
                .into_iter()
                .map(|peer| peer.to_canonical())
                .collect(),
        )
    }

    pub fn trusts(&self, peer: Option<IpAddr>) -> bool {
        match peer {
            _ if self.0.is_empty() => true,
            Some(peer) => self.0.contains(&peer.to_canonical()),
            None => false,
        }
    }
}

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
        self.header.is_some() && self.proxies.trusts(None)
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
            .filter(|_| self.proxies.trusts(socket))
            .and_then(|header| forwarded_peer(headers, header))
    }

    fn ignores_header_from(&self, headers: &HeaderMap, socket: IpAddr) -> bool {
        self.header
            .as_ref()
            .is_some_and(|header| headers.contains_key(header))
            && !self.proxies.trusts(Some(socket))
    }
}

fn forwarded_peer(headers: &HeaderMap, header: &HeaderName) -> Option<IpAddr> {
    headers
        .get(header)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit(',').next())
        .map(str::trim)
        .and_then(|candidate| candidate.parse::<IpAddr>().ok())
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

    #[test]
    fn forwarded_peer_takes_the_rightmost_parseable_entry() {
        [
            (Some("203.0.113.7, 198.51.100.4"), Some("198.51.100.4")),
            (Some("  192.0.2.1  "), Some("192.0.2.1")),
            (Some("not-an-ip"), None),
            (None, None),
        ]
        .iter()
        .for_each(|&(header, expected)| {
            assert_eq!(
                forwarded_peer(&headers(header), &forwarded_for()),
                expected.map(ip),
                "{header:?}"
            );
        });
    }

    #[test]
    fn an_empty_allowlist_trusts_every_peer() {
        let anyone = TrustedProxies::default();
        assert!(anyone.trusts(Some(ip("203.0.113.7"))));
        assert!(anyone.trusts(None));
    }

    #[test]
    fn the_header_applies_only_to_a_peer_on_the_allowlist() {
        let proxy = ip("127.0.0.1");
        let forged = headers(Some("198.51.100.4"));
        let trust = ProxyTrust::new(Some(forwarded_for()), TrustedProxies::new([proxy]));
        let peer = |socket| trust.client_peer(&forged, Some(socket));

        assert_eq!(
            peer(proxy),
            Some(ip("198.51.100.4")),
            "a request relayed by the listed proxy is limited by the address the proxy recorded"
        );
        assert_eq!(
            peer(ip("203.0.113.7")),
            Some(ip("203.0.113.7")),
            "a client reaching the knot directly forged the header and must answer for its socket"
        );
    }

    #[test]
    fn a_caller_with_a_socket_address_gets_the_same_answer_without_an_option() {
        let listed = TrustedProxies::new([ip("127.0.0.1")]);
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
    fn a_listed_ipv4_proxy_still_matches_the_v4_mapped_address_a_dual_stack_listener_reports() {
        let mapped = ip("::ffff:127.0.0.1");
        assert!(
            TrustedProxies::new([ip("127.0.0.1")]).trusts(Some(mapped)),
            "binding [::] turns an IPv4 proxy into ::ffff:127.0.0.1 and the allowlist must still match it"
        );
        assert!(
            TrustedProxies::new([mapped]).trusts(Some(ip("127.0.0.1"))),
            "an operator who writes the mapped form must match a plain IPv4 peer too"
        );
        assert!(
            !TrustedProxies::new([ip("127.0.0.1")]).trusts(Some(ip("::1"))),
            "the IPv6 loopback is a different address from the IPv4 one"
        );
    }

    #[test]
    fn client_peer_falls_back_to_the_socket_whenever_no_header_applies() {
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
        });
    }

    #[test]
    fn one_address_gets_one_bucket_however_the_listener_spelled_it() {
        let trust = ProxyTrust::default();
        assert_eq!(
            trust.client_peer(&headers(None), Some(ip("::ffff:203.0.113.7"))),
            trust.client_peer(&headers(None), Some(ip("203.0.113.7"))),
            "a v4-mapped socket and the plain v4 address are one client, so they share a key"
        );
    }

    #[test]
    fn client_peer_reports_no_peer_when_an_allowlist_leaves_it_with_neither_source() {
        let trust = ProxyTrust::new(
            Some(forwarded_for()),
            TrustedProxies::new([ip("127.0.0.1")]),
        );
        assert_eq!(
            trust.client_peer(&headers(Some("198.51.100.4")), None),
            None,
            "with no socket to check against the allowlist there is no client to key on"
        );
    }

    #[test]
    fn the_peer_key_separates_an_ignored_header_from_a_request_that_never_sent_one() {
        let listed = ProxyTrust::new(
            Some(forwarded_for()),
            TrustedProxies::new([ip("127.0.0.1")]),
        );
        assert_eq!(
            listed.peer_key(&headers(Some("198.51.100.4")), Some(ip("203.0.113.7"))),
            PeerKey::SocketWithIgnoredHeader(ip("203.0.113.7")),
            "an unlisted peer sent the header, which is the address an operator has to see"
        );
        assert_eq!(
            listed.peer_key(&headers(None), Some(ip("203.0.113.7"))),
            PeerKey::Socket(ip("203.0.113.7")),
            "a request without the header says nothing about the allowlist"
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
    }

    #[test]
    fn only_an_ignored_header_reports_an_address_to_warn_about() {
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
                "{key:?} is not a misconfigured allowlist"
            );
        });
    }

    #[test]
    fn an_ignored_header_still_keys_the_peer_on_its_socket() {
        let listed = ProxyTrust::new(
            Some(forwarded_for()),
            TrustedProxies::new([ip("127.0.0.1")]),
        );
        let forged = headers(Some("198.51.100.4"));
        assert_eq!(
            listed.client_peer(&forged, Some(ip("203.0.113.7"))),
            Some(ip("203.0.113.7"))
        );
        assert_eq!(
            listed.client_peer_of(&forged, ip("::ffff:203.0.113.7")),
            ip("203.0.113.7"),
            "the reported address stays canonical so the warning and the bucket agree"
        );
    }

    #[test]
    fn a_populated_allowlist_trusts_only_the_addresses_it_lists() {
        let proxies = TrustedProxies::new([ip("127.0.0.1"), ip("::1")]);
        assert!(proxies.trusts(Some(ip("127.0.0.1"))));
        assert!(proxies.trusts(Some(ip("::1"))));
        assert!(
            !proxies.trusts(Some(ip("203.0.113.7"))),
            "a client reaching the knot directly would pick its own rate-limit bucket"
        );
        assert!(
            !proxies.trusts(None),
            "a peer of None has no address to match against the list"
        );
    }

    #[test]
    fn only_a_header_without_an_allowlist_trusts_any_peer() {
        let listed = TrustedProxies::new([ip("127.0.0.1")]);
        assert!(
            ProxyTrust::new(Some(forwarded_for()), TrustedProxies::default()).trusts_any_peer()
        );
        assert!(!ProxyTrust::new(Some(forwarded_for()), listed).trusts_any_peer());
        assert!(!ProxyTrust::default().trusts_any_peer());
    }
}
