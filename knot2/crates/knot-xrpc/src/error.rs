use axum::Json;
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use serde_json::json;

#[derive(Debug, Clone)]
pub struct XrpcError {
    status: StatusCode,
    error: &'static str,
    message: String,
}

impl XrpcError {
    fn new(status: StatusCode, error: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            error,
            message: message.into(),
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "InvalidRequest", message)
    }

    pub(crate) fn named(
        status: StatusCode,
        error: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self::new(status, error, message)
    }

    pub fn auth_required(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "AuthenticationRequired", message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "Forbidden", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "NotFound", message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "Conflict", message)
    }

    pub fn request_too_large(message: impl Into<String>) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, "RequestTooLarge", message)
    }

    pub fn warming(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "ProjectionWarming",
            message,
        )
    }

    pub fn upstream_unavailable(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "UpstreamUnavailable",
            message,
        )
    }

    pub fn rate_limited(message: impl Into<String>) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, "RateLimitExceeded", message)
    }

    pub fn overloaded(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "Overloaded", message)
    }

    pub fn bad_gateway(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, "UpstreamFailure", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "InternalError", message)
    }

    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }

    pub(crate) fn from_status(status: StatusCode, message: impl Into<String>) -> Self {
        let error = match status {
            StatusCode::BAD_REQUEST => "InvalidRequest",
            StatusCode::UNAUTHORIZED => "AuthenticationRequired",
            StatusCode::FORBIDDEN => "Forbidden",
            StatusCode::NOT_FOUND => "NotFound",
            StatusCode::CONFLICT => "Conflict",
            StatusCode::PAYLOAD_TOO_LARGE => "RequestTooLarge",
            StatusCode::UNSUPPORTED_MEDIA_TYPE => "UnsupportedMediaType",
            StatusCode::TOO_MANY_REQUESTS => "RateLimitExceeded",
            StatusCode::SERVICE_UNAVAILABLE => "Overloaded",
            StatusCode::BAD_GATEWAY => "UpstreamFailure",
            _ => return Self::internal(message),
        };
        Self::new(status, error, message)
    }
}

impl std::fmt::Display for XrpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.error, self.message)
    }
}

impl From<knot_git::GitError> for XrpcError {
    fn from(error: knot_git::GitError) -> Self {
        use knot_git::GitError;
        let message = error.to_string();
        match error {
            GitError::AlreadyExists(_) => Self::conflict(message),
            GitError::AtomicRefs(_) => Self::conflict(message),
            GitError::UnsafeRepoDid(_) | GitError::ReservedDid(_) => Self::invalid_request(message),
            GitError::DepthExceeded(_) => Self::invalid_request(message),
            GitError::ArchiveTooLarge { .. } => Self::request_too_large(message),
            GitError::Selection(_) => Self::overloaded(message),
            // Every oid passed to the object database here came from a ref this
            // knot already resolved or a tree it already read, so a miss means
            // the repository is missing an object it references. A handler that
            // looks up something the caller named reports its own named error
            // before it ever gets an oid.
            GitError::ObjectNotFound(_)
            | GitError::Open { .. }
            | GitError::Create { .. }
            | GitError::Remove { .. }
            | GitError::Reference { .. }
            | GitError::Fsync { .. }
            | GitError::Write { .. }
            | GitError::RevWalk(_)
            | GitError::RemoveObject { .. }
            | GitError::Corrupt { .. }
            | GitError::ObjectType { .. }
            | GitError::Decode(_)
            | GitError::Backend(_)
            | GitError::Staging(_)
            | GitError::Config { .. }
            | GitError::Maintenance(_) => Self::internal(message),
        }
    }
}

impl From<knot_git::ApplyError> for XrpcError {
    fn from(error: knot_git::ApplyError) -> Self {
        use knot_git::ApplyError;
        match error {
            ApplyError::TooLarge => Self::request_too_large(error.to_string()),
            ApplyError::Git(inner) => inner.into(),
        }
    }
}

impl From<knot_cob::CobError> for XrpcError {
    fn from(error: knot_cob::CobError) -> Self {
        use knot_cob::CobError;
        match error {
            CobError::Contended(_) | CobError::StaleTip { .. } => Self::conflict(error.to_string()),
            other => Self::internal(other.to_string()),
        }
    }
}

impl From<knot_index::IndexError> for XrpcError {
    fn from(error: knot_index::IndexError) -> Self {
        use knot_index::IndexError;
        match error {
            IndexError::Git(git) => git.into(),
            IndexError::Cob(cob) => cob.into(),
            other => Self::internal(other.to_string()),
        }
    }
}

impl From<knot_secrets::SecretsError> for XrpcError {
    fn from(error: knot_secrets::SecretsError) -> Self {
        use knot_secrets::SecretsError;
        match error {
            SecretsError::Occupied(_) => Self::conflict(error.to_string()),
            other => Self::internal(other.to_string()),
        }
    }
}

impl From<knot_cobs::RegistryError> for XrpcError {
    fn from(error: knot_cobs::RegistryError) -> Self {
        use knot_cobs::RegistryError;
        match error {
            RegistryError::AlreadyRegistered { .. }
            | RegistryError::RepoMismatch { .. }
            | RegistryError::RkeyTaken { .. }
            | RegistryError::OwnerMoved { .. } => Self::conflict(error.to_string()),
            RegistryError::NotRegistered { .. } | RegistryError::NotHosted { .. } => {
                Self::not_found(error.to_string())
            }
            RegistryError::Cob(cob) => cob.into(),
        }
    }
}

impl From<knot_atproto::AtprotoError> for XrpcError {
    fn from(error: knot_atproto::AtprotoError) -> Self {
        use knot_atproto::AtprotoError;
        if error.is_transient() {
            return Self::upstream_unavailable(error.to_string());
        }
        match error {
            AtprotoError::Resolve(_) => Self::invalid_request(error.to_string()),
            AtprotoError::PlcSubmit { .. } => Self::bad_gateway(error.to_string()),
            other => Self::internal(other.to_string()),
        }
    }
}

impl From<knot_atproto::IdentityError> for XrpcError {
    fn from(error: knot_atproto::IdentityError) -> Self {
        Self::internal(error.to_string())
    }
}

impl From<knot_pack::PackError> for XrpcError {
    fn from(error: knot_pack::PackError) -> Self {
        Self::from_status(error.http_status(), error.to_string())
    }
}

impl From<knot_pack::FetchError> for XrpcError {
    fn from(error: knot_pack::FetchError) -> Self {
        use knot_pack::FetchError;
        let message = error.to_string();
        match error {
            FetchError::Url(_) => Self::invalid_request(message),
            FetchError::Network(_) => Self::upstream_unavailable(message),
            FetchError::Status(_) | FetchError::Protocol(_) | FetchError::Remote(_) => {
                Self::bad_gateway(message)
            }
            FetchError::PackTooLarge { .. } => Self::request_too_large(message),
            FetchError::Pack(pack) => pack.into(),
        }
    }
}

impl IntoResponse for XrpcError {
    fn into_response(self) -> Response {
        match self.status {
            StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS => tracing::warn!(
                status = self.status.as_u16(),
                error = self.error,
                message = %self.message,
                "request rejected"
            ),
            StatusCode::UNAUTHORIZED => tracing::debug!(
                error = self.error,
                message = %self.message,
                "request unauthenticated"
            ),
            _ => {}
        }
        (
            self.status,
            Json(json!({ "error": self.error, "message": self.message })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::XrpcError;
    use axum::response::IntoResponse;
    use http::StatusCode;

    #[test]
    fn each_tag_maps_to_its_status_in_the_class_and_through_into_response() {
        let cases: &[(XrpcError, StatusCode, &str)] = &[
            (
                XrpcError::invalid_request("x"),
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
            ),
            (
                XrpcError::auth_required("x"),
                StatusCode::UNAUTHORIZED,
                "AuthenticationRequired",
            ),
            (
                XrpcError::forbidden("x"),
                StatusCode::FORBIDDEN,
                "Forbidden",
            ),
            (XrpcError::not_found("x"), StatusCode::NOT_FOUND, "NotFound"),
            (XrpcError::conflict("x"), StatusCode::CONFLICT, "Conflict"),
            (
                XrpcError::request_too_large("x"),
                StatusCode::PAYLOAD_TOO_LARGE,
                "RequestTooLarge",
            ),
            (
                XrpcError::rate_limited("x"),
                StatusCode::TOO_MANY_REQUESTS,
                "RateLimitExceeded",
            ),
            (
                XrpcError::warming("x"),
                StatusCode::SERVICE_UNAVAILABLE,
                "ProjectionWarming",
            ),
            (
                XrpcError::upstream_unavailable("x"),
                StatusCode::SERVICE_UNAVAILABLE,
                "UpstreamUnavailable",
            ),
            (
                XrpcError::overloaded("x"),
                StatusCode::SERVICE_UNAVAILABLE,
                "Overloaded",
            ),
            (
                XrpcError::bad_gateway("x"),
                StatusCode::BAD_GATEWAY,
                "UpstreamFailure",
            ),
            (
                XrpcError::internal("x"),
                StatusCode::INTERNAL_SERVER_ERROR,
                "InternalError",
            ),
        ];
        cases.iter().for_each(|(error, status, tag)| {
            assert_eq!((error.status, error.error), (*status, *tag));
            assert_eq!(
                error.clone().into_response().status(),
                *status,
                "into_response serves the mapped status for {tag}"
            );
        });
    }

    #[test]
    fn a_domain_error_maps_to_the_status_that_names_whose_fault_it_is() {
        let oid = knot_types::Oid::from_hex(&"a".repeat(40)).unwrap();
        let cases: Vec<(XrpcError, StatusCode, &str)> = vec![
            (
                knot_git::GitError::ObjectNotFound(oid).into(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "these oids come from refs and trees this knot resolved itself, \
                 and every read lexicon names its own not-found error for what the caller asked for",
            ),
            (
                knot_git::GitError::Corrupt {
                    oid,
                    message: "truncated".to_string(),
                }
                .into(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "a corrupt repository on this knot isn't the caller's fault to fix",
            ),
            (
                knot_git::GitError::AtomicRefs("lost".to_string()).into(),
                StatusCode::CONFLICT,
                "losing a ref race is a conflict",
            ),
            (
                knot_git::GitError::AlreadyExists("/scallop".into()).into(),
                StatusCode::CONFLICT,
                "creating a repository that exists is a conflict",
            ),
            (
                knot_pack::FetchError::Remote("gone".to_string()).into(),
                StatusCode::BAD_GATEWAY,
                "a fetch failure at the upstream during fork sync is the upstream's fault",
            ),
            (
                knot_git::ApplyError::Git(knot_git::GitError::AtomicRefs("lost".to_string()))
                    .into(),
                StatusCode::CONFLICT,
                "wrapping a git error in ApplyError mustn't downgrade it to a generic fault",
            ),
            (
                knot_index::IndexError::Git(knot_git::GitError::AlreadyExists("/whelk".into()))
                    .into(),
                StatusCode::CONFLICT,
                "wrapping a git error in IndexError mustn't downgrade it either",
            ),
        ];
        cases.iter().for_each(|(error, status, why)| {
            assert_eq!(error.status(), *status, "{why}");
        });
    }
}
