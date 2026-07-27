use axum::Router;
use axum::extract::Request;
use axum::handler::Handler;
use axum::middleware::{Next, from_fn};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use http::StatusCode;

use crate::protocol::NegotiatedProtocol;

pub struct ZeroRttSafe<H> {
    handler: H,
}

impl<H> ZeroRttSafe<H> {
    pub fn new(handler: H) -> Self {
        Self { handler }
    }
}

pub struct RequiresFullHandshake {
    router: Router,
}

impl RequiresFullHandshake {
    pub fn new(router: Router) -> Self {
        Self {
            router: router.layer(from_fn(reject_early_writes)),
        }
    }

    pub(crate) fn into_router(self) -> Router {
        self.router
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EarlyData {
    Yes,
    No,
}

impl EarlyData {
    pub fn is_early(self) -> bool {
        matches!(self, EarlyData::Yes)
    }
}

const EARLY_DATA_HEADER: &str = "early-data";

pub struct ZeroRttRoutes {
    router: Router,
    count: usize,
}

impl ZeroRttRoutes {
    pub fn new() -> Self {
        Self {
            router: Router::new(),
            count: 0,
        }
    }

    pub fn get<H, T>(mut self, path: &str, handler: ZeroRttSafe<H>) -> Self
    where
        H: Handler<T, ()>,
        T: 'static,
    {
        self.router = self.router.route(path, get(handler.handler));
        self.count += 1;
        self
    }

    pub fn into_router(self) -> Router {
        self.router
    }

    pub(crate) fn early_data_policy(&self) -> EarlyDataPolicy {
        match self.count {
            0 => EarlyDataPolicy::Disabled,
            _ => EarlyDataPolicy::Enabled,
        }
    }
}

impl Default for ZeroRttRoutes {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EarlyDataPolicy {
    Disabled,
    Enabled,
}

impl EarlyDataPolicy {
    pub(crate) fn max_early_data_size(self) -> u32 {
        match self {
            EarlyDataPolicy::Disabled => 0,
            // rustls takes 0 or u32::MAX,
            // and quinn unwraps errors, quite unfortunate.
            //
            // The 0 above is hard-off marker, for when no route opted in.
            EarlyDataPolicy::Enabled => u32::MAX,
        }
    }
}

pub(crate) async fn tag_from_header(mut request: Request, next: Next) -> Response {
    if request.extensions().get::<EarlyData>().is_none() {
        let early = request
            .headers()
            .get(EARLY_DATA_HEADER)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.trim() == "1");
        request
            .extensions_mut()
            .insert(if early { EarlyData::Yes } else { EarlyData::No });
    }
    next.run(request).await
}

async fn reject_early_writes(request: Request, next: Next) -> Response {
    let early = request
        .extensions()
        .get::<EarlyData>()
        .copied()
        .unwrap_or(EarlyData::Yes);
    if early.is_early() {
        let protocol = request
            .extensions()
            .get::<NegotiatedProtocol>()
            .map(|protocol| protocol.as_str())
            .unwrap_or("unknown");
        tracing::debug!(
            protocol,
            path = %request.uri().path(),
            "refused an early-data request to a full-handshake route with 425"
        );
        return too_early();
    }
    next.run(request).await
}

fn too_early() -> Response {
    (
        StatusCode::TOO_EARLY,
        "this route refuses early data and needs a completed handshake",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_safe_set_keeps_early_data_disabled() {
        assert_eq!(
            ZeroRttRoutes::new().early_data_policy(),
            EarlyDataPolicy::Disabled
        );
        assert_eq!(EarlyDataPolicy::Disabled.max_early_data_size(), 0);
    }

    #[test]
    fn a_proven_safe_set_unlocks_a_nonzero_early_data_size() {
        let routes = ZeroRttRoutes::new().get("/info/refs", ZeroRttSafe::new(|| async { "ok" }));
        assert_eq!(routes.early_data_policy(), EarlyDataPolicy::Enabled);
        assert_eq!(EarlyDataPolicy::Enabled.max_early_data_size(), u32::MAX);
    }

    #[tokio::test]
    async fn a_full_handshake_route_fails_closed_when_the_request_is_unclassified() {
        use axum::body::Body;
        use axum::routing::post;
        use http::{Request, StatusCode};
        use tower::ServiceExt;

        let router = Router::new()
            .route("/git-upload-pack", post(|| async { "pack" }))
            .layer(from_fn(reject_early_writes));
        let request = Request::post("/git-upload-pack")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            router.oneshot(request).await.unwrap().status(),
            StatusCode::TOO_EARLY,
            "a write whose early-data status was never tagged must fail closed with 425"
        );
    }
}
