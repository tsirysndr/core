use std::collections::BTreeSet;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::str::FromStr;

use ipnet::IpNet;

pub const MAX_HOPS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ProxyNet(IpNet);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProxyNetError {
    #[error("`{0}` isn't an IP address or a CIDR block")]
    Unreadable(String),
    #[error("a blank entry isn't an IP address or a CIDR block, so drop it or fill it in")]
    Blank,
}

impl ProxyNet {
    fn contains(self, peer: IpAddr) -> bool {
        match peer.to_canonical() {
            IpAddr::V4(v4) => {
                self.0.contains(&IpAddr::V4(v4))
                    || self.0.contains(&IpAddr::V6(v4.to_ipv6_mapped()))
            }
            v6 => self.0.contains(&v6),
        }
    }
}

impl FromStr for ProxyNet {
    type Err = ProxyNetError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim() {
            "" => Err(ProxyNetError::Blank),
            entry => entry
                .parse::<IpNet>()
                .ok()
                .or_else(|| entry.parse::<IpAddr>().ok().map(IpNet::from))
                .map(|net| Self(canonical(net)))
                .ok_or_else(|| ProxyNetError::Unreadable(entry.to_owned())),
        }
    }
}

fn canonical(net: IpNet) -> IpNet {
    match net {
        IpNet::V6(v6) => match (v6.network().to_canonical(), v6.prefix_len()) {
            (v4 @ IpAddr::V4(_), len @ 96..) => IpNet::new_assert(v4, len - 96),
            _ => net,
        },
        IpNet::V4(_) => net,
    }
}

fn chain_address(entry: &str) -> Option<IpAddr> {
    entry
        .parse::<IpAddr>()
        .ok()
        .or_else(|| entry.parse::<SocketAddr>().ok().map(|hop| hop.ip()))
        .or_else(|| {
            entry
                .strip_prefix('[')
                .and_then(|rest| rest.strip_suffix(']'))
                .and_then(|inner| inner.parse::<Ipv6Addr>().ok())
                .map(IpAddr::V6)
        })
}

pub fn comma_separated(raw: &str) -> Result<Vec<String>, Infallible> {
    Ok(raw
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect())
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustedProxies(BTreeSet<ProxyNet>);

impl TrustedProxies {
    pub fn parse<'a>(entries: impl IntoIterator<Item = &'a str>) -> Result<Self, ProxyNetError> {
        entries
            .into_iter()
            .map(ProxyNet::from_str)
            .collect::<Result<BTreeSet<_>, _>>()
            .map(Self)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(&self, peer: IpAddr) -> bool {
        self.0.iter().any(|net| net.contains(peer))
    }

    pub fn rightmost_untrusted<'a, I>(&self, chain: I) -> Option<IpAddr>
    where
        I: IntoIterator<Item = &'a str>,
        I::IntoIter: DoubleEndedIterator,
    {
        chain
            .into_iter()
            .rev()
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .take(MAX_HOPS)
            .find_map(|entry| match chain_address(entry) {
                Some(address) if self.contains(address) => None,
                parsed => Some(parsed),
            })
            .flatten()
            .map(|address| address.to_canonical())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }

    fn trusting<'a>(entries: impl IntoIterator<Item = &'a str>) -> TrustedProxies {
        TrustedProxies::parse(entries).unwrap()
    }

    fn client(proxies: &TrustedProxies, chain: &str) -> Option<IpAddr> {
        proxies.rightmost_untrusted(chain.split(','))
    }

    #[test]
    fn rightmost_untrusted_takes_the_rightmost_entry_with_an_empty_list() {
        let anyone = TrustedProxies::default();
        [
            ("203.0.113.7, 198.51.100.4", Some("198.51.100.4")),
            ("  192.0.2.1  ", Some("192.0.2.1")),
            ("not-an-ip", None),
            ("", None),
        ]
        .iter()
        .for_each(|&(chain, expected)| {
            assert_eq!(client(&anyone, chain), expected.map(ip), "{chain:?}");
        });
    }

    #[test]
    fn rightmost_untrusted_steps_over_every_listed_hop_and_stops_where_it_cant_parse() {
        let proxies = trusting(["198.51.100.4"]);
        [
            (
                "203.0.113.7, 198.51.100.4",
                Some("203.0.113.7"),
                "the hop left of the listed proxy is the client",
            ),
            (
                "203.0.113.7, 198.51.100.4, 198.51.100.4",
                Some("203.0.113.7"),
                "two listed hops in a row are both stepped over",
            ),
            (
                "203.0.113.7, 192.0.2.1, 198.51.100.4",
                Some("192.0.2.1"),
                "rightmost_untrusted stops at the first unlisted hop, even with a client still further left",
            ),
            (
                "198.51.100.4",
                None,
                "rightmost_untrusted won't find a client in a chain of listed hops alone",
            ),
            (
                "203.0.113.7, not-an-ip, 198.51.100.4",
                None,
                "rightmost_untrusted stops at an entry it can't parse, because a client can write anything left of the proxy",
            ),
            (
                "203.0.113.7, 198.51.100.4,",
                Some("203.0.113.7"),
                "a stray comma mustn't cost the header, since a client can't claim anything through a blank",
            ),
            (
                ",203.0.113.7,, 198.51.100.4",
                Some("203.0.113.7"),
                "a blank anywhere else in the chain reads the same way",
            ),
            (
                "203.0.113.7:4321, 198.51.100.4",
                Some("203.0.113.7"),
                "some proxies write the hop with the port it connected from",
            ),
            (
                "[2001:db8::1]:443, 198.51.100.4",
                Some("2001:db8::1"),
                "and bracket an IPv6 hop when they do",
            ),
            (
                "[2001:db8::1], 198.51.100.4",
                Some("2001:db8::1"),
                "brackets turn up without a port too",
            ),
            (
                "2001:db8::1, 198.51.100.4",
                Some("2001:db8::1"),
                "a bare IPv6 hop doesn't need unwrapping",
            ),
        ]
        .iter()
        .for_each(|&(chain, expected, why)| {
            assert_eq!(client(&proxies, chain), expected.map(ip), "{why}: {chain:?}");
        });
        assert_eq!(
            client(
                &trusting(["198.51.100.0/24"]),
                "203.0.113.7, 198.51.100.9, 198.51.100.10"
            ),
            Some(ip("203.0.113.7")),
            "one CIDR entry covers every hop inside the block"
        );
    }

    #[test]
    fn rightmost_untrusted_reads_only_the_last_max_hops_entries() {
        let proxies = trusting(["198.51.100.4"]);
        let padded = |hops| {
            std::iter::once("203.0.113.7")
                .chain(std::iter::repeat_n("198.51.100.4", hops))
                .collect::<Vec<_>>()
                .join(",")
        };
        assert_eq!(
            client(&proxies, &padded(MAX_HOPS - 1)),
            Some(ip("203.0.113.7")),
            "a chain within MAX_HOPS still reaches the client behind every listed hop"
        );
        assert_eq!(
            client(&proxies, &padded(MAX_HOPS)),
            None,
            "the peer answers for the address it connected from when the last MAX_HOPS entries are all listed hops"
        );
    }

    #[test]
    fn an_entry_covers_every_address_it_spans_in_either_spelling() {
        [
            (&["198.51.100.0/24"][..], "198.51.100.4", true, "a CIDR entry covers the addresses inside it"),
            (&["198.51.100.0/24"], "198.51.100.255", true, "up to the last address in the block"),
            (&["198.51.100.0/24"], "198.51.101.1", false, "and stops at the block boundary"),
            (&["2001:db8::/32"], "2001:db8::dead:beef", true, "an IPv6 block reads the same way"),
            (&["2001:db8::/32"], "2001:db9::1", false, "and stops at its boundary too"),
            (&["2001:db8::/32"], "198.51.100.4", false, "an IPv6 block that isn't v4-mapped won't cover an IPv4 address"),
            (&["::ffff:198.51.100.0/120"], "198.51.100.4", true, "a v4-mapped block covers the plain v4 addresses inside it"),
            (&["::ffff:198.51.100.0/120"], "::ffff:198.51.100.4", true, "in either spelling"),
            (&["::ffff:198.51.100.0/120"], "198.51.101.4", false, "and stops at the folded block boundary"),
            (&["::ffff:0:0/96"], "203.0.113.7", true, "the whole v4-mapped range folds to every IPv4 address"),
            (&["::ffff:0:0/95"], "203.0.113.7", true, "the match has to try the mapped spelling too, because a prefix under 96 keeps the block in IPv6, where the plain v4 spelling of a peer would miss it"),
            (&["::/0"], "203.0.113.7", true, "an operator who lists every IPv6 address has listed every v4-mapped address with it"),
            (&["127.0.0.1", " ::1 "], "127.0.0.1", true, "a bare address is a single host, and its entry may be padded"),
            (&["127.0.0.1", " ::1 "], "::ffff:127.0.0.1", true, "which a dual-stack listener may report mapped"),
            (&["127.0.0.1", " ::1 "], "::1", true, "the IPv6 loopback is its own entry"),
            (&["127.0.0.1", " ::1 "], "127.0.0.2", false, "and the host next door is outside all of them"),
            (&[], "203.0.113.7", false, "a caller that reads an empty list as trusting every peer has to say so itself, since rightmost_untrusted would step over every entry and never find a client"),
        ]
        .iter()
        .for_each(|&(entries, peer, expected, why)| {
            assert_eq!(
                trusting(entries.iter().copied()).contains(ip(peer)),
                expected,
                "{why}: {entries:?} against {peer}"
            );
        });
        assert!(TrustedProxies::default().is_empty());
    }

    #[test]
    fn parse_refuses_an_entry_it_cant_read_and_quotes_it() {
        assert_eq!(
            TrustedProxies::parse(["127.0.0.1:5555"])
                .unwrap_err()
                .to_string(),
            "`127.0.0.1:5555` isn't an IP address or a CIDR block",
        );
        [vec![""], vec![" "], vec!["127.0.0.1", "\t"]]
            .into_iter()
            .for_each(|entries| {
                assert_eq!(
                    TrustedProxies::parse(entries.iter().copied()),
                    Err(ProxyNetError::Blank),
                    "discarding the blank would leave a list the operator filled in reading as empty. A caller is free to read an empty list as trusting every peer: {entries:?}"
                );
            });
    }

    #[test]
    fn comma_separated_discards_the_gaps_a_separator_leaves_behind() {
        assert_eq!(
            comma_separated("127.0.0.1, 173.245.48.0/20,").unwrap(),
            vec!["127.0.0.1".to_owned(), "173.245.48.0/20".to_owned()],
            "comma_separated mustn't leave a blank for parse to refuse, since a trailing separator belongs to the format"
        );
        assert!(comma_separated("").unwrap().is_empty());
    }
}
