use std::error::Error as StdError;
use std::fmt;
use std::io;
use std::net::SocketAddr;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use crate::host::{PrivateHostReason, classify_ip};

pub struct PrivateAddressFilter {
    allow_private: bool,
}

impl PrivateAddressFilter {
    pub fn new(allow_private: bool) -> Self {
        Self { allow_private }
    }
}

impl Resolve for PrivateAddressFilter {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_private = self.allow_private;
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let resolved: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            partition_safe(host, allow_private, resolved)
        })
    }
}

fn partition_safe(
    host: String,
    allow_private: bool,
    resolved: Vec<SocketAddr>,
) -> Result<Addrs, Box<dyn StdError + Send + Sync>> {
    if !allow_private && let Some(reason) = resolved.iter().find_map(|sa| classify_ip(&sa.ip())) {
        return Err(Box::new(BlockedAddressError { host, reason }));
    }
    if resolved.is_empty() {
        return Err(Box::new(io::Error::other(format!(
            "no resolvable addresses for {host}"
        ))));
    }
    Ok(Box::new(resolved.into_iter()))
}

#[derive(Debug)]
pub(crate) struct BlockedAddressError {
    host: String,
    reason: PrivateHostReason,
}

impl fmt::Display for BlockedAddressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "dns resolution for {} returned {} address",
            self.host, self.reason,
        )
    }
}

impl StdError for BlockedAddressError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn sa(ip: &str, port: u16) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), port)
    }

    #[test]
    fn blocks_when_any_resolved_address_is_private_under_strict() {
        let mixed = vec![sa("8.8.8.8", 0), sa("127.0.0.1", 0)];
        let res = partition_safe("mixed.example".into(), false, mixed);
        assert!(res.is_err(), "any private address must fail strict resolve");
    }

    #[test]
    fn allows_all_when_permissive() {
        let mixed = vec![sa("8.8.8.8", 0), sa("127.0.0.1", 0)];
        let res = partition_safe("mixed.example".into(), true, mixed).expect("permissive");
        let collected: Vec<SocketAddr> = res.collect();
        assert_eq!(collected.len(), 2);
    }

    #[test]
    fn permits_public_only_resolution_under_strict() {
        let public = vec![sa("8.8.8.8", 0), sa("1.1.1.1", 0)];
        let res = partition_safe("public.example".into(), false, public).expect("public");
        let collected: Vec<SocketAddr> = res.collect();
        assert_eq!(collected.len(), 2);
    }

    #[test]
    fn empty_resolution_is_an_error() {
        let res = partition_safe("nx.example".into(), false, vec![]);
        assert!(res.is_err(), "empty address list must surface an error");
    }

    #[test]
    fn classify_ip_matches_url_classifier() {
        assert!(classify_ip(&IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))).is_some());
        assert!(classify_ip(&IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))).is_none());
    }
}
