use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use knot_runtime::{Clock, HttpTransport};
use knot_types::AccountDid;

use crate::XrpcState;

pub(crate) const VERSION_ROUTE: &str = "/xrpc/sh.tangled.knot.version";
pub(crate) const OWNER_ROUTE: &str = "/xrpc/sh.tangled.owner";
pub(crate) const HEALTH_ROUTE: &str = "/xrpc/_health";

const WIRE_VERSION: &str = "v1.15.0";

#[derive(Serialize)]
struct VersionWire {
    version: &'static str,
    capabilities: [&'static str; 2],
}

#[derive(Serialize)]
struct HealthWire {
    version: String,
}

pub(crate) async fn health<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
) -> Response {
    if let Some(lfs) = state.lfs.as_ref()
        && !lfs.ready().await
    {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "lfs store is unreachable or not writable",
        )
            .into_response();
    }
    Json(HealthWire {
        version: format!("knot {}", env!("CARGO_PKG_VERSION")),
    })
    .into_response()
}

#[derive(Serialize)]
struct OwnerWire {
    owner: AccountDid,
}

pub(crate) async fn version() -> Response {
    Json(VersionWire {
        version: WIRE_VERSION,
        capabilities: ["knot-acl", "repo-did-input"],
    })
    .into_response()
}

pub(crate) async fn owner<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
) -> Response {
    Json(OwnerWire {
        owner: state.service_owner.clone(),
    })
    .into_response()
}
