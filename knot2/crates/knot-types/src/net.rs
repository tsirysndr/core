use std::net::IpAddr;

use http::HeaderMap;
use http::header::AsHeaderName;

pub fn forwarded_peer<K: AsHeaderName>(headers: &HeaderMap, header: K) -> Option<IpAddr> {
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

    use http::HeaderName;

    fn headers(value: Option<&str>) -> HeaderMap {
        value
            .map(|value| {
                let mut map = HeaderMap::new();
                map.insert(
                    HeaderName::from_bytes(b"x-forwarded-for").unwrap(),
                    value.parse().unwrap(),
                );
                map
            })
            .unwrap_or_default()
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
                forwarded_peer(&headers(header), "x-forwarded-for"),
                expected.map(|ip| ip.parse::<IpAddr>().unwrap()),
                "{header:?}"
            );
        });
    }
}
