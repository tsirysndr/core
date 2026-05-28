use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use jacquard_common::BosStr;
use jacquard_common::types::did::Did;
use jacquard_common::types::nsid::Nsid;
use thiserror::Error;
use url::{Host, Url};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct KnotHost(Url);

#[derive(Clone, Debug, Error)]
pub enum KnotHostError {
    #[error("empty host")]
    Empty,
    #[error("parse: {0}")]
    Parse(url::ParseError),
    #[error("scheme must be http or https, got {0}")]
    BadScheme(String),
    #[error("host has no authority")]
    NoAuthority,
}

impl KnotHost {
    pub fn parse(raw: &str) -> Result<Self, KnotHostError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(KnotHostError::Empty);
        }
        let candidate = if trimmed.contains("://") {
            trimmed.to_owned()
        } else {
            format!("https://{trimmed}")
        };
        let mut url = Url::parse(&candidate).map_err(KnotHostError::Parse)?;
        match url.scheme() {
            "http" | "https" => {}
            other => return Err(KnotHostError::BadScheme(other.to_owned())),
        }
        if url.host().is_none() {
            return Err(KnotHostError::NoAuthority);
        }
        url.set_path("/");
        url.set_query(None);
        url.set_fragment(None);
        let _ = url.set_username("");
        let _ = url.set_password(None);
        Ok(Self(url))
    }

    pub fn url(&self) -> &Url {
        &self.0
    }

    pub fn xrpc_url<S: BosStr + AsRef<str>>(&self, nsid: &Nsid<S>) -> Url {
        let mut url = self.0.clone();
        url.set_path(&format!("/xrpc/{}", nsid.as_ref()));
        url
    }

    pub fn private_literal_reason(&self) -> Option<PrivateHostReason> {
        match self.0.host()? {
            Host::Ipv4(ip) => classify_v4(ip),
            Host::Ipv6(ip) => classify_v6(ip),
            Host::Domain(name) if is_loopback_domain(name) => Some(PrivateHostReason::Loopback),
            Host::Domain(_) => None,
        }
    }
}

pub fn classify_ip(ip: &IpAddr) -> Option<PrivateHostReason> {
    match ip {
        IpAddr::V4(v4) => classify_v4(*v4),
        IpAddr::V6(v6) => classify_v6(*v6),
    }
}

fn is_loopback_domain(name: &str) -> bool {
    name.eq_ignore_ascii_case("localhost")
        || name
            .rsplit_once('.')
            .is_some_and(|(_, label)| label.eq_ignore_ascii_case("localhost"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrivateHostReason {
    Loopback,
    Private,
    LinkLocal,
    Unspecified,
    Multicast,
    Broadcast,
    Documentation,
    UniqueLocal,
}

impl std::fmt::Display for PrivateHostReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Loopback => "loopback",
            Self::Private => "private",
            Self::LinkLocal => "link-local",
            Self::Unspecified => "unspecified",
            Self::Multicast => "multicast",
            Self::Broadcast => "broadcast",
            Self::Documentation => "documentation",
            Self::UniqueLocal => "unique-local",
        })
    }
}

fn classify_v4(ip: Ipv4Addr) -> Option<PrivateHostReason> {
    if ip.is_loopback() {
        Some(PrivateHostReason::Loopback)
    } else if ip.is_private() {
        Some(PrivateHostReason::Private)
    } else if ip.is_link_local() {
        Some(PrivateHostReason::LinkLocal)
    } else if ip.is_unspecified() {
        Some(PrivateHostReason::Unspecified)
    } else if ip.is_broadcast() {
        Some(PrivateHostReason::Broadcast)
    } else if ip.is_multicast() {
        Some(PrivateHostReason::Multicast)
    } else if ip.is_documentation() {
        Some(PrivateHostReason::Documentation)
    } else if is_v4_carrier_grade_nat(ip) {
        Some(PrivateHostReason::Private)
    } else {
        None
    }
}

fn is_v4_carrier_grade_nat(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && (0x40..=0x7f).contains(&b)
}

fn classify_v6(ip: Ipv6Addr) -> Option<PrivateHostReason> {
    if ip.is_loopback() {
        Some(PrivateHostReason::Loopback)
    } else if ip.is_unspecified() {
        Some(PrivateHostReason::Unspecified)
    } else if ip.is_multicast() {
        Some(PrivateHostReason::Multicast)
    } else if is_v6_link_local(ip) {
        Some(PrivateHostReason::LinkLocal)
    } else if is_v6_unique_local(ip) {
        Some(PrivateHostReason::UniqueLocal)
    } else if let Some(v4) = ip.to_ipv4_mapped() {
        classify_v4(v4)
    } else {
        None
    }
}

fn is_v6_link_local(ip: Ipv6Addr) -> bool {
    ip.segments()[0] & 0xffc0 == 0xfe80
}

fn is_v6_unique_local(ip: Ipv6Addr) -> bool {
    ip.segments()[0] & 0xfe00 == 0xfc00
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoSlug(String);

#[derive(Clone, Debug, Error)]
pub enum RepoSlugError {
    #[error("empty repo name")]
    EmptyName,
    #[error("repo name contains slash: {0}")]
    NameHasSlash(String),
}

impl RepoSlug {
    pub fn new<S: BosStr + AsRef<str>>(did: &Did<S>, name: &str) -> Result<Self, RepoSlugError> {
        if name.is_empty() {
            return Err(RepoSlugError::EmptyName);
        }
        if name.contains('/') {
            return Err(RepoSlugError::NameHasSlash(name.to_owned()));
        }
        Ok(Self(format!("{}/{}", did.as_ref(), name)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jacquard_common::DefaultStr;

    fn did(s: &'static str) -> Did<DefaultStr> {
        Did::new_static(s).unwrap()
    }

    fn nsid(s: &'static str) -> Nsid<DefaultStr> {
        Nsid::new_static(s).unwrap()
    }

    #[test]
    fn parses_bare_host_as_https() {
        let host = KnotHost::parse("oyster.cafe").unwrap();
        assert_eq!(host.url().as_str(), "https://oyster.cafe/");
    }

    #[test]
    fn parses_explicit_scheme() {
        let host = KnotHost::parse("http://127.0.0.1:5555").unwrap();
        assert_eq!(host.url().as_str(), "http://127.0.0.1:5555/");
    }

    #[test]
    fn strips_path_query_fragment() {
        let host = KnotHost::parse("https://nel.pet/some/path?x=1#y").unwrap();
        assert_eq!(host.url().as_str(), "https://nel.pet/");
    }

    #[test]
    fn strips_userinfo() {
        let host = KnotHost::parse("https://attacker:secret@oyster.cafe/").unwrap();
        assert_eq!(host.url().as_str(), "https://oyster.cafe/");
        assert!(host.url().username().is_empty());
        assert!(host.url().password().is_none());
    }

    #[test]
    fn strips_userinfo_only_username() {
        let host = KnotHost::parse("https://nel@nel.pet").unwrap();
        assert_eq!(host.url().as_str(), "https://nel.pet/");
        assert!(host.url().username().is_empty());
    }

    #[test]
    fn rejects_bad_scheme() {
        let err = KnotHost::parse("ftp://oyster.cafe").unwrap_err();
        assert!(matches!(err, KnotHostError::BadScheme(s) if s == "ftp"));
    }

    #[test]
    fn rejects_empty() {
        assert!(matches!(
            KnotHost::parse("   ").unwrap_err(),
            KnotHostError::Empty,
        ));
    }

    #[test]
    fn xrpc_url_appends_nsid() {
        let host = KnotHost::parse("oyster.cafe").unwrap();
        let url = host.xrpc_url(&nsid("sh.tangled.repo.blob"));
        assert_eq!(
            url.as_str(),
            "https://oyster.cafe/xrpc/sh.tangled.repo.blob",
        );
    }

    #[test]
    fn slug_joins_did_and_name() {
        let slug = RepoSlug::new(&did("did:plc:squid"), "barnacle").unwrap();
        assert_eq!(slug.as_str(), "did:plc:squid/barnacle");
    }

    #[test]
    fn slug_rejects_slash_in_name() {
        assert!(matches!(
            RepoSlug::new(&did("did:plc:squid"), "bad/name").unwrap_err(),
            RepoSlugError::NameHasSlash(s) if s == "bad/name",
        ));
    }

    #[test]
    fn slug_rejects_empty_name() {
        assert!(matches!(
            RepoSlug::new(&did("did:plc:squid"), "").unwrap_err(),
            RepoSlugError::EmptyName,
        ));
    }

    #[test]
    fn flags_loopback_v4() {
        let host = KnotHost::parse("http://127.0.0.1").unwrap();
        assert_eq!(
            host.private_literal_reason(),
            Some(PrivateHostReason::Loopback),
        );
    }

    #[test]
    fn flags_private_rfc1918() {
        for raw in ["http://10.0.0.1", "http://192.168.1.1", "http://172.16.0.1"] {
            let host = KnotHost::parse(raw).unwrap();
            assert_eq!(
                host.private_literal_reason(),
                Some(PrivateHostReason::Private),
                "{raw}",
            );
        }
    }

    #[test]
    fn flags_link_local_v4() {
        let host = KnotHost::parse("http://169.254.169.254").unwrap();
        assert_eq!(
            host.private_literal_reason(),
            Some(PrivateHostReason::LinkLocal),
        );
    }

    #[test]
    fn flags_carrier_grade_nat() {
        let host = KnotHost::parse("http://100.64.0.1").unwrap();
        assert_eq!(
            host.private_literal_reason(),
            Some(PrivateHostReason::Private),
        );
    }

    #[test]
    fn flags_loopback_v6() {
        let host = KnotHost::parse("http://[::1]").unwrap();
        assert_eq!(
            host.private_literal_reason(),
            Some(PrivateHostReason::Loopback),
        );
    }

    #[test]
    fn flags_link_local_v6() {
        let host = KnotHost::parse("http://[fe80::1]").unwrap();
        assert_eq!(
            host.private_literal_reason(),
            Some(PrivateHostReason::LinkLocal),
        );
    }

    #[test]
    fn flags_unique_local_v6() {
        let host = KnotHost::parse("http://[fc00::1]").unwrap();
        assert_eq!(
            host.private_literal_reason(),
            Some(PrivateHostReason::UniqueLocal),
        );
    }

    #[test]
    fn flags_v4_mapped_v6() {
        let host = KnotHost::parse("http://[::ffff:10.0.0.1]").unwrap();
        assert_eq!(
            host.private_literal_reason(),
            Some(PrivateHostReason::Private),
        );
    }

    #[test]
    fn allows_public_v4() {
        let host = KnotHost::parse("http://93.184.216.34").unwrap();
        assert_eq!(host.private_literal_reason(), None);
    }

    #[test]
    fn dns_names_are_not_classified() {
        let host = KnotHost::parse("https://oyster.cafe").unwrap();
        assert_eq!(host.private_literal_reason(), None);
    }

    #[test]
    fn flags_localhost_domain() {
        let host = KnotHost::parse("http://localhost").unwrap();
        assert_eq!(
            host.private_literal_reason(),
            Some(PrivateHostReason::Loopback),
        );
    }

    #[test]
    fn flags_localhost_subdomain() {
        let host = KnotHost::parse("http://internal.localhost").unwrap();
        assert_eq!(
            host.private_literal_reason(),
            Some(PrivateHostReason::Loopback),
        );
    }

    #[test]
    fn flags_localhost_case_insensitive() {
        let host = KnotHost::parse("http://LOCALHOST").unwrap();
        assert_eq!(
            host.private_literal_reason(),
            Some(PrivateHostReason::Loopback),
        );
    }

    #[test]
    fn does_not_flag_domain_merely_containing_localhost() {
        let host = KnotHost::parse("https://localhost.attacker.example").unwrap();
        assert_eq!(host.private_literal_reason(), None);
    }
}
