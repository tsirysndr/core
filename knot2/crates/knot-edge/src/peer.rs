use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts};
use http::request::Parts;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketPeer(Option<IpAddr>);

impl SocketPeer {
    pub fn ip(self) -> Option<IpAddr> {
        self.0
    }
}

impl<S: Send + Sync> FromRequestParts<S> for SocketPeer {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|connect| connect.0.ip()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use std::net::Ipv4Addr;

    #[tokio::test]
    async fn the_extractor_reads_connect_info_and_tolerates_its_absence() {
        let with = {
            let mut request = http::Request::builder().body(Body::empty()).unwrap();
            request
                .extensions_mut()
                .insert(ConnectInfo(SocketAddr::from(([203, 0, 113, 7], 443))));
            let (mut parts, _) = request.into_parts();
            SocketPeer::from_request_parts(&mut parts, &())
                .await
                .unwrap()
        };
        assert_eq!(with.ip(), Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))));

        let without = {
            let request = http::Request::builder().body(Body::empty()).unwrap();
            let (mut parts, _) = request.into_parts();
            SocketPeer::from_request_parts(&mut parts, &())
                .await
                .unwrap()
        };
        assert_eq!(without.ip(), None);
    }
}
