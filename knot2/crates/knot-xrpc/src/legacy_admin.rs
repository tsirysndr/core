use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::middleware::from_fn_with_state;
use axum::response::Response;
use axum::routing::post;
use http::HeaderMap;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use knot_cobs::Grant;
use knot_runtime::{Clock, HttpTransport};

use crate::error::XrpcError;
use crate::members::{SubjectInput, grant_membership};
use crate::{XrpcState, basic_credentials, decode, enforce_pre_auth_limit};

pub const ADD_MEMBER_ROUTE: &str = "/admin/addMember";

const BASIC_USER: &str = "admin";

#[derive(Debug, thiserror::Error)]
#[error("legacy admin secret mustn't be empty")]
pub struct EmptySecret;

pub struct LegacyAdminSecret(Zeroizing<String>);

impl LegacyAdminSecret {
    pub fn new(value: &str) -> Result<Self, EmptySecret> {
        let value = value.trim();
        match value.is_empty() {
            true => Err(EmptySecret),
            false => Ok(Self(Zeroizing::new(value.to_string()))),
        }
    }

    fn authorize(&self, headers: &HeaderMap) -> Result<(), XrpcError> {
        let denied = || XrpcError::auth_required("invalid admin credentials");
        let credentials = headers
            .get(http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(basic_credentials)
            .ok_or_else(denied)?;
        let admitted = credentials.user.matches(BASIC_USER)
            && bool::from(credentials.password.as_bytes().ct_eq(self.0.as_bytes()));
        match admitted {
            true => Ok(()),
            false => Err(denied()),
        }
    }
}

struct LegacyAdmin<H, C> {
    state: Arc<XrpcState<H, C>>,
    secret: LegacyAdminSecret,
}

pub fn router<H: HttpTransport, C: Clock>(
    state: Arc<XrpcState<H, C>>,
    secret: LegacyAdminSecret,
) -> Router {
    let limits = state.byte_limits.body.get();
    let limiter = Arc::clone(&state);
    Router::new()
        .route(ADD_MEMBER_ROUTE, post(add_member::<H, C>))
        .layer(DefaultBodyLimit::max(limits))
        .layer(from_fn_with_state(limiter, enforce_pre_auth_limit::<H, C>))
        .with_state(Arc::new(LegacyAdmin { state, secret }))
}

async fn add_member<H: HttpTransport, C: Clock>(
    State(admin): State<Arc<LegacyAdmin<H, C>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, XrpcError> {
    admin.secret.authorize(&headers)?;
    let SubjectInput { subject } = decode(&body)?;
    tracing::warn!(
        route = ADD_MEMBER_ROUTE,
        %subject,
        "legacy admin route authorized a member grant"
    );
    grant_membership(
        &admin.state,
        Grant {
            subject,
            added_by: admin.state.service_owner.clone(),
            created_at: admin.state.now(),
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    use base64::Engine;

    fn header(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            value.parse().expect("header is ascii"),
        );
        headers
    }

    fn basic(scheme: &str, user: &str, password: &str) -> HeaderMap {
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(format!("{user}:{password}").as_bytes());
        header(&format!("{scheme} {encoded}"))
    }

    #[test]
    fn a_trimmed_secret_admits_only_the_admin_user_sending_it_exactly() {
        assert!(LegacyAdminSecret::new("   ").is_err());
        let secret = LegacyAdminSecret::new("\tnekomilk2\n").expect("secret is non-empty");

        let admitted =
            ["Basic", "basic", "BASIC"].map(|scheme| basic(scheme, "admin", "nekomilk2"));
        let refused = [
            basic("Basic", "admin", "\tnekomilk2\n"),
            basic("Basic", "admin", "nope"),
            basic("Basic", "root", "nekomilk2"),
            basic("Basic", "admin", ""),
            header("Basic !!!not-base64!!!"),
            header("Bearer nekomilk2"),
            header("nekomilk2"),
            HeaderMap::new(),
        ];
        assert!(
            admitted
                .iter()
                .all(|headers| secret.authorize(headers).is_ok()),
            "authorize admits the trimmed secret under any case of the Basic scheme"
        );
        assert!(
            refused
                .iter()
                .all(|headers| secret.authorize(headers).is_err()),
            "authorize refuses an untrimmed, wrong, empty or malformed credential"
        );
    }
}
