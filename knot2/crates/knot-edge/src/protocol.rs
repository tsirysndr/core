use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use http::Version;
use tracing::Instrument;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegotiatedProtocol {
    H1,
    H2,
    H3,
}

impl NegotiatedProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            NegotiatedProtocol::H1 => "h1",
            NegotiatedProtocol::H2 => "h2",
            NegotiatedProtocol::H3 => "h3",
        }
    }

    pub fn from_version(version: Version) -> Option<Self> {
        match version {
            Version::HTTP_10 | Version::HTTP_11 => Some(NegotiatedProtocol::H1),
            Version::HTTP_2 => Some(NegotiatedProtocol::H2),
            Version::HTTP_3 => Some(NegotiatedProtocol::H3),
            _ => None,
        }
    }
}

pub(crate) async fn tag(mut request: Request, next: Next) -> Response {
    let protocol = NegotiatedProtocol::from_version(request.version());
    if let Some(protocol) = protocol {
        request.extensions_mut().insert(protocol);
    }
    let span = tracing::debug_span!(
        "request",
        protocol = protocol
            .map(NegotiatedProtocol::as_str)
            .unwrap_or("unknown")
    );
    next.run(request).instrument(span).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_http_version_maps_to_its_negotiated_protocol() {
        assert_eq!(
            NegotiatedProtocol::from_version(Version::HTTP_11),
            Some(NegotiatedProtocol::H1)
        );
        assert_eq!(
            NegotiatedProtocol::from_version(Version::HTTP_10),
            Some(NegotiatedProtocol::H1)
        );
        assert_eq!(
            NegotiatedProtocol::from_version(Version::HTTP_2),
            Some(NegotiatedProtocol::H2)
        );
        assert_eq!(
            NegotiatedProtocol::from_version(Version::HTTP_3),
            Some(NegotiatedProtocol::H3)
        );
        assert_eq!(NegotiatedProtocol::from_version(Version::HTTP_09), None);
    }

    #[test]
    fn the_label_is_stable_for_logging_and_metrics() {
        assert_eq!(NegotiatedProtocol::H1.as_str(), "h1");
        assert_eq!(NegotiatedProtocol::H2.as_str(), "h2");
        assert_eq!(NegotiatedProtocol::H3.as_str(), "h3");
    }
}
